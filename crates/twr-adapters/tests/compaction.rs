//! Context compaction, replayed from recordings. No real runtime runs here.
//!
//! Fixtures `tests/fixtures/{claude,codex}/compact_*.both.jsonl` are real sessions recorded on
//! 2026-10-21 by a probe that drives the CLI the way twr does (Claude Code 2.1.304 with
//! `--model haiku`, the arguments of `claude_code::build_args`; Codex 0.162.1 `app-server`,
//! model `gpt-6-luna`), both directions interleaved (`>> ` written to stdin, `<< ` read from
//! stdout), scrubbed (paths, ids of the account). The probe also asked Claude for its context
//! usage (`control_request get_context_usage`) around the compaction; twr does not, and the
//! replay skips those requests.
//!
//! - Claude `compact_manual`: a turn, `/compact`, a turn that still knows the codeword.
//!   `compact_auto`: `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=3`: the second turn's auto-compaction
//!   fails (`too_few_groups`), the third's succeeds. `compact_disabled`: `DISABLE_COMPACT=1`,
//!   `/compact` answered with a message. `compact_while_running`: `/compact` written while a
//!   turn runs (Claude runs it after). `compact_resume`: a new process resuming the compacted
//!   session.
//! - Codex `compact_manual`: a turn, `thread/compact/start`, a turn. `compact_auto`:
//!   `model_auto_compact_token_limit` low, the third turn compacts first. `compact_auto_midturn`:
//!   the compaction comes after a tool's output, inside the turn. `compact_while_running`:
//!   `thread/compact/start` written during a turn interrupts it (what twr never does: it queues
//!   the compaction). `compact_resume`: `thread/resume` of a compacted thread.
//!
//! Grok Build's compaction has no recording yet (`grok_build.rs`, `synthetic_compaction`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use twr_adapters::{claude_code, codex};
use twr_core::adapter::SpawnConfig;
use twr_core::event::*;

fn fixture(runtime: &str, name: &str) -> Vec<(String, String)> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(runtime)
        .join(format!("{name}.both.jsonl"));
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| (l[..2].to_string(), l[3..].to_string()))
        .collect()
}

fn parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

/// `kind@turn`, without text deltas, task and usage reports.
fn shape<'a>(events: impl IntoIterator<Item = (&'a EventBody, Option<u64>)>) -> Vec<String> {
    events
        .into_iter()
        .filter(|(b, _)| {
            !matches!(
                b,
                EventBody::AgentText(AgentText { delta: true, .. })
                    | EventBody::TaskUpdated(_)
                    | EventBody::RuntimeLimits(_)
            )
        })
        .map(|(b, t)| match t {
            Some(t) => format!("{}@{t}", b.kind()),
            None => b.kind().to_string(),
        })
        .collect()
}

fn compacted(bodies: &[&EventBody]) -> Vec<ContextCompacted> {
    bodies
        .iter()
        .filter_map(|b| match b {
            EventBody::ContextCompacted(c) => Some(c.clone()),
            _ => None,
        })
        .collect()
}

fn ended(bodies: &[&EventBody]) -> Vec<TurnEnded> {
    bodies
        .iter()
        .filter_map(|b| match b {
            EventBody::TurnEnded(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

// --- Claude Code -----------------------------------------------------------------------------

/// Replays a Claude fixture: `/compact` is written as the translator's compaction message,
/// any other user message as a prompt (each checked against the recording), turns from 1.
fn claude(name: &str) -> Vec<claude_code::Emitted> {
    let mut tr = claude_code::Translator::new(claude_code::TranslatorOptions::default());
    let mut events = Vec::new();
    let mut turn = 1;
    for (dir, line) in fixture("claude", name) {
        if dir == "<<" {
            let out = tr.translate(&line);
            assert!(out.fatal.is_none(), "fatal at {line}");
            events.extend(out.events);
            continue;
        }
        let rec = parse(&line);
        match rec["type"].as_str() {
            Some("user") => {
                let text = rec["message"]["content"].as_str().unwrap();
                let ours = match text {
                    "/compact" => tr.compact_message(turn),
                    _ => tr.user_message(text, turn),
                };
                turn += 1;
                assert_eq!(parse(&ours), rec, "stdin line differs from the recording");
            }
            // The probe's own reads of the context usage.
            Some("control_request") => {
                assert_eq!(rec["request"]["subtype"], "get_context_usage");
            }
            other => panic!("unexpected stdin line {other:?}"),
        }
    }
    events
}

fn claude_bodies(events: &[claude_code::Emitted]) -> Vec<&EventBody> {
    events.iter().map(|e| &e.body).collect()
}

/// A manual `/compact`: its compaction (start, boundary) is the turn written for it, which
/// ends `completed` like any turn although it ran no agent call; the summary Claude
/// writes into the session shows nowhere; the context is what the compaction left.
#[test]
fn claude_manual_compact_is_a_turn_of_its_own() {
    let events = claude("compact_manual");
    let shape = shape(events.iter().map(|e| (&e.body, e.turn)));
    assert_eq!(
        shape,
        [
            "thread.started",
            "agent.text@1",
            "turn.ended@1",
            "context.compacting@2",
            "context.compacted@2",
            "turn.ended@2",
            "agent.text@3",
            "turn.ended@3",
        ]
    );
    let bodies = claude_bodies(&events);
    let c = &compacted(&bodies)[0];
    assert_eq!(c.trigger, "manual");
    assert_eq!((c.pre_tokens, c.post_tokens), (Some(32_800), Some(12_311)));
    assert_eq!(c.duration_ms, Some(4_554));
    assert!(matches!(
        bodies.iter().find(|b| matches!(b, EventBody::ContextCompacting(_))),
        Some(EventBody::ContextCompacting(ContextCompacting { trigger: Some(t), .. })) if t == "manual"
    ));
    // Nothing of the summary or the replayed command output is text of the agent's.
    let texts: Vec<&str> = bodies
        .iter()
        .filter_map(|b| match b {
            EventBody::AgentText(t) if t.is_final => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["ok", "PAPAYA"]);
    let ends = ended(&bodies);
    assert!(ends.iter().all(|e| e.result == TurnResult::Completed));
    let window = Some(1_000_000);
    assert_eq!(
        ends[0].context,
        Some(ContextSize {
            tokens: 2 + 20_101 + 12_693,
            window
        })
    );
    // The compaction's turn ran no agent call: its context is what the compaction left.
    assert_eq!(
        ends[1].context,
        Some(ContextSize {
            tokens: 12_311,
            window
        })
    );
    assert_eq!(ends[1].usage.as_ref().map(|u| u.input), Some(0));
    assert_eq!(
        ends[2].context,
        Some(ContextSize {
            tokens: 2 + 19_850 + 15_275,
            window
        })
    );
}

/// An auto-compaction inside a turn: a failed one (`compact_result: failed`) is recorded with
/// its error, a successful one with its sizes; both `auto`, in the turn they happened in.
#[test]
fn claude_auto_compaction_in_a_turn() {
    let events = claude("compact_auto");
    assert_eq!(
        shape(events.iter().map(|e| (&e.body, e.turn))),
        [
            "thread.started",
            "agent.text@1",
            "turn.ended@1",
            "context.compacting@2",
            "context.compacted@2",
            "agent.text@2",
            "turn.ended@2",
            "context.compacting@3",
            "context.compacted@3",
            "agent.text@3",
            "turn.ended@3",
        ]
    );
    let bodies = claude_bodies(&events);
    let c = compacted(&bodies);
    assert_eq!(c[0].trigger, "auto");
    assert_eq!(c[0].error.as_deref(), Some("too_few_groups"));
    assert_eq!(
        c[0].line(),
        "context compaction failed (auto): too_few_groups"
    );
    assert_eq!(c[1].trigger, "auto");
    assert_eq!(
        (c[1].pre_tokens, c[1].post_tokens),
        (Some(34_878), Some(12_726))
    );
    assert_eq!(c[1].error, None);
    assert_eq!(
        c[1].line(),
        "context compacted (auto) · 34.9k tokens → 12.7k"
    );
    // The turn called the model after the compaction: its context is that call's.
    assert_eq!(
        ended(&bodies)[2].context.map(|c| c.tokens),
        Some(2 + 20_038 + 15_275)
    );
}

/// With compaction turned off, `/compact` is answered with a message: an ordinary turn with
/// that text and no compaction.
#[test]
fn claude_compact_turned_off_says_so() {
    let events = claude("compact_disabled");
    let bodies = claude_bodies(&events);
    assert!(compacted(&bodies).is_empty());
    assert!(!bodies
        .iter()
        .any(|b| matches!(b, EventBody::ContextCompacting(_))));
    assert!(events.iter().any(|e| e.turn == Some(2)
        && matches!(&e.body, EventBody::AgentText(t) if t.is_final
            && t.text.contains("/compact isn't available"))));
    assert_eq!(
        ended(&bodies).iter().map(|e| e.result).collect::<Vec<_>>(),
        [TurnResult::Completed; 3]
    );
}

/// `/compact` written while a turn runs: Claude runs it after that turn, as the next one.
#[test]
fn claude_compact_written_during_a_turn_runs_after_it() {
    let events = claude("compact_while_running");
    assert_eq!(
        shape(events.iter().map(|e| (&e.body, e.turn))),
        [
            "thread.started",
            "agent.text@1",
            "turn.ended@1",
            "context.compacting@2",
            "context.compacted@2",
            "turn.ended@2",
        ]
    );
    assert_eq!(compacted(&claude_bodies(&events))[0].trigger, "manual");
}

/// A process resuming a compacted session goes on as any resumed one.
#[test]
fn claude_resume_after_compaction() {
    let events = claude("compact_resume");
    assert_eq!(
        shape(events.iter().map(|e| (&e.body, e.turn))),
        ["thread.started", "agent.text@1", "turn.ended@1"]
    );
}

// --- Codex -----------------------------------------------------------------------------------

/// A request of a method the probe never sent (the background inventory, the usage read):
/// answered as an older server would, unknown.
fn optional(request: &Value) -> bool {
    matches!(
        request["method"].as_str(),
        Some("thread/backgroundTerminals/list" | "account/rateLimits/read")
    )
}

/// Replays a Codex fixture: the translator's own lines for `turn/start` (`send`) and
/// `thread/compact/start` (`compact`) are checked by method and id against the recording
/// (the probe opened the thread with params of its own).
fn codex(name: &str) -> Vec<codex::Emitted> {
    let cfg = SpawnConfig {
        thread: "T1".into(),
        cwd: PathBuf::from("/tmp/sandbox-codex"),
        mode: "read-only".into(),
        // `compact_resume` resumes `compact_manual`'s thread.
        resume_session: (name == "compact_resume")
            .then(|| "01a15e57-1aad-7a92-9a4b-02c6c3b9dcf4".to_string()),
        ..Default::default()
    };
    let open = codex::thread_params(&cfg).unwrap();
    let mut tr = codex::Translator::new(codex::TranslatorOptions::default(), open);
    let mut outbox: VecDeque<String> = VecDeque::from([tr.start()]);
    let mut events = Vec::new();
    let mut turn = 1;
    for (dir, line) in fixture("codex", name) {
        if dir == "<<" {
            let out = tr.translate(&line);
            assert!(out.fatal.is_none(), "fatal at {line}: {:?}", out.fatal);
            events.extend(out.events);
            outbox.extend(out.replies);
            continue;
        }
        let rec = parse(&line);
        while outbox.front().is_some_and(|l| optional(&parse(l))) {
            let request = parse(&outbox.pop_front().unwrap());
            let answer =
                json!({"id": request["id"], "error": {"code": -32601, "message": "unknown"}});
            let out = tr.translate(&answer.to_string());
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
                Some("thread/compact/start") => {
                    outbox.extend(tr.compact(turn));
                    turn += 1;
                    // Asked for while a turn runs: twr sends it once that turn ended.
                    if outbox.is_empty() {
                        assert!(tr.busy());
                        continue;
                    }
                }
                other => panic!("no action produces {other:?}"),
            }
        }
        let ours = parse(&outbox.pop_front().expect("a line of the adapter's"));
        assert_eq!(ours["method"], rec["method"], "at {line}");
        assert_eq!(ours["id"], rec["id"], "at {line}");
        if rec["method"] == "thread/compact/start" {
            assert_eq!(ours["params"], rec["params"]);
        }
    }
    events
}

fn codex_bodies(events: &[codex::Emitted]) -> Vec<&EventBody> {
    events.iter().map(|e| &e.body).collect()
}

/// `thread/compact/start` is a turn twr started though no answer names it: the `turn/started`
/// after it is twr's turn, which its compaction and end belong to (not a runtime turn of
/// Codex's own); the estimate in between is the context after.
#[test]
fn codex_manual_compaction_is_the_turn_twr_opened() {
    let events = codex("compact_manual");
    assert_eq!(
        shape(events.iter().map(|e| (&e.body, e.turn))),
        [
            "thread.started",
            "agent.text@1",
            "turn.ended@1",
            "context.compacting@2",
            "context.compacted@2",
            "turn.ended@2",
            "agent.text@3",
            "turn.ended@3",
        ]
    );
    let bodies = codex_bodies(&events);
    assert!(!bodies
        .iter()
        .any(|b| matches!(b, EventBody::TurnStarted(_))));
    let c = &compacted(&bodies)[0];
    assert_eq!(c.trigger, "manual");
    assert_eq!((c.pre_tokens, c.post_tokens), (Some(18_548), Some(11_954)));
    let ends = ended(&bodies);
    assert!(ends.iter().all(|e| e.result == TurnResult::Completed));
    let window = Some(258_400);
    assert_eq!(
        ends[1].context,
        Some(ContextSize {
            tokens: 11_954,
            window
        })
    );
    assert_eq!(
        ends[2].context,
        Some(ContextSize {
            tokens: 18_600,
            window
        })
    );
}

/// Auto-compactions: at a turn's start and after a tool's output, both `auto`, inside the
/// turn.
#[test]
fn codex_auto_compaction_in_a_turn() {
    for (name, turns, pre, post) in [
        ("compact_auto", &[3, 3][..], 19_125, 12_019),
        // After the tool's output in turn 1, then at turn 2's start.
        ("compact_auto_midturn", &[1, 1, 2, 2][..], 18_594, 12_038),
    ] {
        let events = codex(name);
        let at: Vec<Option<u64>> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.body,
                    EventBody::ContextCompacting(_) | EventBody::ContextCompacted(_)
                )
            })
            .map(|e| e.turn)
            .collect();
        let turns: Vec<Option<u64>> = turns.iter().map(|t| Some(*t)).collect();
        assert_eq!(at, turns, "{name}");
        let c = &compacted(&codex_bodies(&events))[0];
        assert_eq!(c.trigger, "auto", "{name}");
        assert_eq!(
            (c.pre_tokens, c.post_tokens),
            (Some(pre), Some(post)),
            "{name}"
        );
    }
}

/// A translator with its thread open (`th`), as the unit tests open one.
fn open_codex() -> codex::Translator {
    let mut tr = codex::Translator::new(
        codex::TranslatorOptions::default(),
        codex::Open {
            method: "thread/start",
            params: json!({"approvalPolicy": "untrusted", "approvalsReviewer": "user",
                "sandbox": "workspace-write"}),
            mode_note: None,
            added_instructions: None,
        },
    );
    tr.start();
    tr.translate(r#"{"id":1,"result":{}}"#);
    let out = tr.translate(
        r#"{"id":2,"result":{"thread":{"id":"th"},"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":{"type":"workspaceWrite","networkAccess":false}}}"#,
    );
    assert!(out.fatal.is_none(), "{out:?}");
    tr
}

fn methods(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|l| parse(l)["method"].as_str().map(str::to_string))
        .filter(|m| !optional(&json!({ "method": m })))
        .collect()
}

/// A compaction asked for while a turn runs waits for it (sent then, Codex would interrupt
/// the turn): `thread/compact/start` goes out once that turn completed.
#[test]
fn codex_compaction_waits_for_the_running_turn() {
    let mut tr = open_codex();
    let start = tr.send("hi", 1);
    assert_eq!(methods(&start), ["turn/start"]);
    let id = parse(&start[0])["id"].clone();
    assert!(tr.compact(2).is_empty(), "queued behind turn 1");
    let _ = tr.translate(&json!({"id": id, "result": {"turn": {"id": "tu1"}}}).to_string());
    let done = tr.translate(
        r#"{"method":"turn/completed","params":{"threadId":"th","turn":{"id":"tu1","status":"completed"}}}"#,
    );
    assert_eq!(methods(&done.replies), ["thread/compact/start"]);
    // Its turn: the next `turn/started`, though no answer named it.
    let _ = tr.translate(r#"{"id":4,"result":{}}"#);
    let out =
        tr.translate(r#"{"method":"turn/started","params":{"threadId":"th","turn":{"id":"tu2"}}}"#);
    assert!(out.events.is_empty(), "no turn of Codex's own: {out:?}");
    let out = tr.translate(
        r#"{"method":"turn/completed","params":{"threadId":"th","turn":{"id":"tu2","status":"completed"}}}"#,
    );
    let end = out
        .events
        .iter()
        .find(|e| matches!(e.body, EventBody::TurnEnded(_)))
        .expect("the compaction's turn ends");
    assert_eq!(end.turn, Some(2));
}

/// A refused `thread/compact/start` ends twr's turn `error`, saying why.
#[test]
fn codex_refused_compaction_fails_its_turn() {
    let mut tr = open_codex();
    let lines = tr.compact(1);
    assert_eq!(methods(&lines), ["thread/compact/start"]);
    let req = parse(&lines[0]);
    assert_eq!(req["params"], json!({"threadId": "th"}));
    let out = tr.translate(
        &json!({"id": req["id"], "error": {"code": -32600, "message": "no history to compact"}})
            .to_string(),
    );
    let end = out
        .events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnEnded(t) => Some((e.turn, t.clone())),
            _ => None,
        })
        .expect("the turn ends");
    assert_eq!(end.0, Some(1));
    assert_eq!(end.1.result, TurnResult::Error);
    assert!(end.1.ext.unwrap()["twr"]["error"]
        .as_str()
        .unwrap()
        .contains("no history to compact"));
    assert!(!tr.busy());
}

/// The probe wrote `thread/compact/start` during a turn, which Codex answered by
/// interrupting it; twr queues it instead and sends it once that turn ended (here, as the
/// recorded interruption ended it). The compaction then runs in the turn twr opened for it.
#[test]
fn codex_compaction_asked_during_a_turn_waits_for_it() {
    let events = codex("compact_while_running");
    assert_eq!(
        shape(events.iter().map(|e| (&e.body, e.turn))),
        [
            "thread.started",
            "turn.ended@1",
            "context.compacting@2",
            "context.compacted@2",
            "turn.ended@2",
        ]
    );
    let bodies = codex_bodies(&events);
    assert_eq!(ended(&bodies)[0].result, TurnResult::Interrupted);
    assert_eq!(compacted(&bodies)[0].trigger, "manual");
}

/// A resumed compacted thread goes on as any resumed one.
#[test]
fn codex_resume_after_compaction() {
    let events = codex("compact_resume");
    assert_eq!(
        shape(events.iter().map(|e| (&e.body, e.turn))),
        ["thread.started", "agent.text@1", "turn.ended@1"]
    );
}
