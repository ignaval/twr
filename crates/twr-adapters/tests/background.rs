//! Protocol-shaped background work fixtures; no signed-in runtime required.

use serde_json::{json, Value};
use twr_adapters::{acp, claude_code, codex};
use twr_core::adapter::Answer;
use twr_core::event::*;

fn task(body: &EventBody) -> Option<(&str, bool)> {
    match body {
        EventBody::TaskUpdated(t) => Some((&t.task_id, t.running)),
        _ => None,
    }
}

#[test]
fn claude_tasks_outlive_results_terminal_patches_win_and_resume_opens_a_turn() {
    let mut tr = claude_code::Translator::new(Default::default());
    tr.user_message("start review", 8);
    tr.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    for id in ["shell", "review"] {
        let line =
            json!({"type":"system","subtype":"task_started","task_id":id,"description":"review"})
                .to_string();
        let out = tr.translate(&line);
        assert_eq!(task(&out.events[0].body), Some((id, true)));
        assert_eq!(out.events[0].turn, None);
        assert!(tr.translate(&line).events.is_empty(), "duplicate start");
    }
    let out = tr.translate(r#"{"type":"result","subtype":"success"}"#);
    assert!(matches!(out.events[0].body, EventBody::TurnEnded(_)));
    assert!(!tr.busy());
    let out = tr.translate(
        r#"{"type":"system","subtype":"task_progress","task_id":"review","description":"review"}"#,
    );
    assert_eq!(task(&out.events[0].body), Some(("review", true)));
    assert!(!tr.busy(), "task signals must not open turns");
    let out = tr.translate(r#"{"type":"assistant","parent_tool_use_id":"review-call","message":{"content":[{"type":"tool_use","id":"child","name":"Read","input":{}}]}}"#);
    assert!(out
        .events
        .iter()
        .all(|e| !matches!(e.body, EventBody::TurnStarted(_))));
    for (id, subtype, status) in [
        ("shell", "task_updated", "killed"),
        ("review", "task_notification", "completed"),
    ] {
        let line = json!({"type":"system","subtype":subtype,"task_id":id,"status":status,"patch":{"status":status}}).to_string();
        let out = tr.translate(&line);
        assert_eq!(task(&out.events[0].body), Some((id, false)));
        assert!(tr.translate(&line).events.is_empty());
        assert!(
            tr.translate(
                &json!({"type":"system","subtype":"task_progress","task_id":id}).to_string()
            )
            .events
            .is_empty(),
            "late progress must not resurrect tasks"
        );
    }
    let out = tr.translate(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"review finished"}]}}"#,
    );
    assert!(
        matches!(&out.events[0].body, EventBody::TurnStarted(t) if t.from == TurnFrom::Runtime)
    );
    assert_eq!(
        out.events[0].turn, None,
        "the hub numbers the runtime's turn"
    );
    let out = tr.translate(r#"{"type":"result","subtype":"success"}"#);
    assert_eq!(out.events[0].turn, None);
}

#[test]
fn claude_shell_result_and_legacy_terminal_notification() {
    let mut tr = claude_code::Translator::new(Default::default());
    tr.user_message("run tests", 1);
    tr.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    let out = tr.translate(
        r#"{"type":"user","tool_use_result":{"backgroundTaskId":"b1"},"message":{"content":[]}}"#,
    );
    assert_eq!(task(&out.events[0].body), Some(("b1", true)));
    tr.translate(r#"{"type":"result","subtype":"success"}"#);
    let out = tr.translate(r#"{"type":"user","message":{"content":"<task-notification><task-id>b1</task-id><status>completed</status></task-notification>"}}"#);
    assert_eq!(task(&out.events[0].body), Some(("b1", false)));
    assert!(!tr.busy());
}

#[test]
fn claude_explicit_task_restart_reopens_an_id_but_late_progress_does_not() {
    let mut tr = claude_code::Translator::new(Default::default());
    let start =
        r#"{"type":"system","subtype":"task_started","task_id":"review","description":"review"}"#;
    assert_eq!(
        task(&tr.translate(start).events[0].body),
        Some(("review", true))
    );
    tr.translate(r#"{"type":"system","subtype":"task_notification","task_id":"review","status":"completed"}"#);
    assert!(tr
        .translate(r#"{"type":"system","subtype":"task_progress","task_id":"review"}"#)
        .events
        .is_empty());
    assert_eq!(
        task(&tr.translate(start).events[0].body),
        Some(("review", true))
    );
    assert!(!tr.busy(), "task launches do not open parent turns");
}

/// An autonomous run's turn starts with the model's first message, before its output; its
/// `init` alone opens none (Claude also sends one before a run that never reaches the model).
#[test]
fn claude_autonomous_turn_starts_before_output_and_ignores_child_streams() {
    for init_first in [false, true] {
        let signal =
            r#"{"type":"stream_event","event":{"type":"message_start"},"parent_tool_use_id":null}"#;
        let mut tr = claude_code::Translator::new(Default::default());
        tr.user_message("start review", 5);
        tr.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
        tr.translate(r#"{"type":"result","subtype":"success"}"#);
        assert!(tr.translate(r#"{"type":"stream_event","event":{"type":"message_start"},"parent_tool_use_id":"child"}"#).events.is_empty());
        if init_first {
            assert!(tr
                .translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#)
                .events
                .is_empty());
        }
        let out = tr.translate(signal);
        assert!(
            matches!(&out.events[0].body, EventBody::TurnStarted(t) if t.from == TurnFrom::Runtime)
        );
        assert_eq!(out.events[0].turn, None);
        assert!(tr.busy());
        assert!(
            tr.translate(signal).events.is_empty(),
            "one turn per resume"
        );
        let out = tr.translate(r#"{"type":"result","subtype":"success"}"#);
        assert_eq!(out.events[0].turn, None);
    }
}

fn codex_open() -> codex::Translator {
    let mut tr = codex::Translator::new(
        Default::default(),
        codex::Open {
            method: "thread/start",
            params: json!({"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"workspace-write"}),
            mode_note: None,
        },
    );
    let init: Value = serde_json::from_str(&tr.start()).unwrap();
    assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
    tr.translate(r#"{"id":1,"result":{}}"#);
    let out = tr.translate(r#"{"id":2,"result":{"thread":{"id":"parent"},"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":{"type":"workspaceWrite"}}}"#);
    assert!(out.fatal.is_none(), "{out:?}");
    tr
}

fn runtime_turn_of(body: &EventBody) -> Option<&str> {
    match body {
        EventBody::TurnStarted(t) => runtime_turn(t.ext.as_ref()),
        EventBody::TurnEnded(t) => runtime_turn(t.ext.as_ref()),
        _ => None,
    }
}

fn ended(out: &[claude_code::Emitted]) -> &claude_code::Emitted {
    out.iter()
        .find(|e| matches!(e.body, EventBody::TurnEnded(_)))
        .unwrap()
}

/// The hub alone numbers turns: a runtime's own turn carries no number, its end included, and
/// its start and end carry the same id of its own.
#[test]
fn runtime_turns_carry_no_number_but_an_id_of_their_own() {
    let mut claude = claude_code::Translator::new(Default::default());
    claude.user_message("start review", 1);
    claude.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    let out = claude.translate(r#"{"type":"result","subtype":"success"}"#);
    assert_eq!(runtime_turn_of(&out.events[0].body), None);
    let out = claude.translate(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"finished"}]}}"#,
    );
    assert!(
        matches!(&out.events[0].body, EventBody::TurnStarted(t) if t.from == TurnFrom::Runtime)
    );
    assert_eq!(runtime_turn_of(&out.events[0].body), Some("1"));
    assert!(out.events.iter().all(|e| e.turn.is_none()), "{out:?}");
    let out = claude.translate(r#"{"type":"result","subtype":"success"}"#);
    assert_eq!(out.events[0].turn, None);
    assert_eq!(runtime_turn_of(&out.events[0].body), Some("1"));

    let mut codex = codex_open();
    let out = codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    assert!(
        matches!(&out.events[0].body, EventBody::TurnStarted(t) if t.from == TurnFrom::Runtime)
    );
    assert_eq!(out.events[0].turn, None);
    assert_eq!(runtime_turn_of(&out.events[0].body), Some("auto"));
    let out = codex.translate(
        r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"auto","status":"completed"}}}"#,
    );
    let end = out
        .events
        .iter()
        .find(|e| matches!(e.body, EventBody::TurnEnded(_)))
        .unwrap();
    assert_eq!(end.turn, None);
    assert_eq!(runtime_turn_of(&end.body), Some("auto"));
}

/// A prompt written while Claude runs a turn of its own: that turn still ends under its id,
/// with its own usage (the hub decides where it goes), and the prompt's turn keeps its number
/// and its usage.
#[test]
fn claude_reports_a_runtime_turn_and_the_prompt_sent_during_it_apart() {
    let mut claude = claude_code::Translator::new(Default::default());
    claude.user_message("start review", 1);
    claude.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    claude.translate(r#"{"type":"result","subtype":"success"}"#);
    claude.translate(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"finished"}]}}"#,
    );
    claude.user_message("next", 2);
    let out = claude.translate(
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":10,"output_tokens":5}}"#,
    );
    let end = ended(&out.events);
    assert_eq!((end.turn, runtime_turn_of(&end.body)), (None, Some("1")));
    assert!(matches!(&end.body, EventBody::TurnEnded(t)
        if t.usage.as_ref().map(|u| (u.input, u.output)) == Some((10, 5))));
    claude.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    let out = claude.translate(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"answer"}]}}"#,
    );
    assert!(!out
        .events
        .iter()
        .any(|e| matches!(e.body, EventBody::TurnStarted(_))));
    assert_eq!(out.events[0].turn, Some(2));
    let out = claude.translate(
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":7,"output_tokens":3}}"#,
    );
    let end = ended(&out.events);
    assert_eq!((end.turn, runtime_turn_of(&end.body)), (Some(2), None));
    assert!(matches!(&end.body, EventBody::TurnEnded(t)
        if t.usage.as_ref().map(|u| (u.input, u.output)) == Some((7, 3))));
}

fn tokens(turn: &str, input: u64, output: u64) -> String {
    json!({"method":"thread/tokenUsage/updated","params":{"threadId":"parent","turnId":turn,
        "tokenUsage":{"total":{"inputTokens":input,"outputTokens":output,"totalTokens":input + output}}}})
    .to_string()
}

fn usage_of(e: &codex::Emitted) -> Option<(u64, u64)> {
    match &e.body {
        EventBody::TurnEnded(t) => t.usage.as_ref().map(|u| (u.input, u.output)),
        _ => None,
    }
}

/// Codex starts a turn of its own after twr's `turn/start` went out but before its answer:
/// that turn is not bound to twr's. Once the answer names twr's turn, it is reported as Codex's
/// own (start and held completion under its id), and twr's turn ends under its number. In the
/// other order seen from Codex (twr's own `turn/started` before the answer), nothing changes.
/// Either way the turn's token updates before the answer are its usage, not history.
#[test]
fn codex_turns_started_before_the_turn_start_answer_are_told_apart() {
    for (runtime, ours) in [("auto", "t2"), ("t2", "t2")] {
        let mut codex = codex_open();
        let start = codex.send("next", 2);
        let id = serde_json::from_str::<Value>(&start[0]).unwrap()["id"].clone();
        let started =
            json!({"method":"turn/started","params":{"threadId":"parent","turn":{"id":runtime}}});
        assert!(codex.translate(&started.to_string()).events.is_empty());
        codex.translate(&tokens(runtime, 10, 5));
        let completed = json!({"method":"turn/completed","params":{"threadId":"parent","turn":{"id":runtime,"status":"completed"}}});
        let out = codex.translate(&completed.to_string());
        assert!(out.events.is_empty(), "held until the answer: {out:?}");
        let out = codex.translate(&json!({"id":id,"result":{"turn":{"id":ours}}}).to_string());
        let kinds: Vec<(&str, Option<u64>, Option<&str>)> = out
            .events
            .iter()
            .map(|e| (e.body.kind(), e.turn, runtime_turn_of(&e.body)))
            .collect();
        if runtime == ours {
            assert_eq!(kinds, [("turn.ended", Some(2), None)]);
            assert_eq!(usage_of(&out.events[0]), Some((10, 5)));
        } else {
            assert_eq!(
                kinds,
                [
                    ("turn.started", None, Some("auto")),
                    ("turn.ended", None, Some("auto")),
                ]
            );
            assert_eq!(usage_of(&out.events[1]), Some((10, 5)));
            codex.translate(&tokens("t2", 17, 8));
            let out = codex.translate(
                r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"t2","status":"completed"}}}"#,
            );
            let end = out
                .events
                .iter()
                .find(|e| matches!(e.body, EventBody::TurnEnded(_)))
                .unwrap();
            assert_eq!((end.turn, runtime_turn_of(&end.body)), (Some(2), None));
            assert_eq!(
                usage_of(end),
                Some((7, 3)),
                "Codex's own turn's tokens are not twr's"
            );
        }
    }
}

/// An approval of a turn whose owner the `turn/start` answer has not told yet waits for it.
/// twr's turn: it is that turn's. Codex's own, twr's start having failed: it comes after that
/// turn's start with no turn (the hub gives it the turn then open, Codex's), stays open past
/// the failed turn's end, and its answer reaches Codex.
#[test]
fn codex_approval_of_an_unsettled_turn_waits_for_its_owner() {
    let approval = |turn: &str| {
        json!({"id":50,"method":"item/commandExecution/requestApproval","params":{"threadId":"parent",
            "turnId":turn,"itemId":"rm","command":"rm -rf target"}})
        .to_string()
    };
    let raised = |out: &codex::Translated| {
        out.events.iter().find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some((a.attn_id.clone(), e.turn)),
            _ => None,
        })
    };

    let mut codex = codex_open();
    let start = codex.send("next", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"t2"}}}"#,
    );
    assert!(raised(&codex.translate(&approval("t2"))).is_none());
    let out = codex
        .translate(&json!({"id":request_id(&start[0]),"result":{"turn":{"id":"t2"}}}).to_string());
    assert_eq!(raised(&out).unwrap().1, Some(2));

    let mut codex = codex_open();
    let start = codex.send("next", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    let out = codex.translate(&approval("auto"));
    assert!(raised(&out).is_none(), "held until the answer: {out:?}");
    let out = codex.translate(
        &json!({"id":request_id(&start[0]),"error":{"code":-32000,"message":"busy"}}).to_string(),
    );
    let kinds: Vec<&str> = out.events.iter().map(|e| e.body.kind()).collect();
    assert_eq!(kinds, ["turn.ended", "turn.started", "attention.raised"]);
    let (attn_id, turn) = raised(&out).unwrap();
    assert_eq!(turn, None);
    assert_eq!(codex.pending_attention(), vec![attn_id.clone()]);
    let out = codex.respond(&attn_id, &Answer::new("allow")).unwrap();
    let reply: Value = serde_json::from_str(&out.replies[0]).unwrap();
    assert_eq!(reply["id"], 50);
}

fn summary(out: &codex::Translated) -> Vec<(&str, Option<u64>, Option<&str>)> {
    out.events
        .iter()
        .filter(|e| matches!(e.body.kind(), "turn.started" | "turn.ended"))
        .map(|e| (e.body.kind(), e.turn, runtime_turn_of(&e.body)))
        .collect()
}

fn request_id(line: &str) -> Value {
    serde_json::from_str::<Value>(line).unwrap()["id"].clone()
}

/// twr's `turn/start` fails while a turn Codex started on its own runs: that turn stays the
/// running one, so a prompt sent next waits for it, and its completion ends only itself.
#[test]
fn codex_own_turn_left_by_a_failed_turn_start_keeps_the_next_prompt_waiting() {
    let mut codex = codex_open();
    let first = codex.send("first", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    let out = codex.translate(
        &json!({"id":request_id(&first[0]),"error":{"code":-32000,"message":"busy"}}).to_string(),
    );
    assert_eq!(
        summary(&out),
        [
            ("turn.ended", Some(2), None),
            ("turn.started", None, Some("auto"))
        ]
    );
    assert!(!out.replies.iter().any(|r| r.contains("turn/start")));
    assert!(
        codex.send("second", 3).is_empty(),
        "turn 3 waits for Codex's own turn"
    );
    let out = codex.translate(
        r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"auto","status":"completed"}}}"#,
    );
    assert_eq!(summary(&out), [("turn.ended", None, Some("auto"))]);
    let start = out
        .replies
        .iter()
        .find(|r| r.contains("turn/start"))
        .expect("turn 3 starts once Codex's own turn is over");
    codex.translate(&json!({"id":request_id(start),"result":{"turn":{"id":"t3"}}}).to_string());
    let out = codex.translate(
        r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"t3","status":"completed"}}}"#,
    );
    assert_eq!(summary(&out), [("turn.ended", Some(3), None)]);
}

/// The completion of a turn Codex started on its own never ends a prompt whose `turn/start` has
/// no answer yet, though that prompt's turn has no Codex id to tell them apart.
#[test]
fn codex_own_turn_completion_never_ends_an_unnamed_prompt() {
    let mut codex = codex_open();
    let first = codex.send("first", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    let out = codex
        .translate(&json!({"id":request_id(&first[0]),"result":{"turn":{"id":"t2"}}}).to_string());
    assert_eq!(summary(&out), [("turn.started", None, Some("auto"))]);
    let out = codex.translate(
        r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"t2","status":"completed"}}}"#,
    );
    assert_eq!(summary(&out), [("turn.ended", Some(2), None)]);
    assert_eq!(codex.send("second", 3).len(), 1);
    let out = codex.translate(
        r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"auto","status":"completed"}}}"#,
    );
    assert_eq!(summary(&out), [("turn.ended", None, Some("auto"))]);
}

/// Everything a turn of unknown owner says before the `turn/start` answer waits for it: here a
/// tool call and an async question of a turn Codex started on its own, twr's start having
/// failed. They follow that turn's start with no turn, and the question, still open, is
/// answered by steering that turn.
#[test]
fn codex_held_turn_tool_calls_and_questions_wait_for_its_owner() {
    let mut codex = codex_open();
    let start = codex.send("next", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    let out = codex.translate(r#"{"method":"item/started","params":{"threadId":"parent","turnId":"auto","item":{"type":"commandExecution","id":"c1","command":"ls","status":"inProgress"}}}"#);
    assert!(
        !out.events
            .iter()
            .any(|e| matches!(e.body, EventBody::AgentToolCall(_))),
        "{out:?}"
    );
    let out = codex.translate(r#"{"method":"item/completed","params":{"threadId":"parent","turnId":"auto","item":{"type":"agentMessage","id":"msg_q","text":"","questions":[{"title":"Keep the cache?","options":["yes","no"]}]}}}"#);
    assert!(out.events.is_empty(), "held until the answer: {out:?}");
    let out = codex.translate(
        &json!({"id":request_id(&start[0]),"error":{"code":-32000,"message":"busy"}}).to_string(),
    );
    let kinds: Vec<(&str, Option<u64>)> =
        out.events.iter().map(|e| (e.body.kind(), e.turn)).collect();
    assert_eq!(
        kinds,
        [
            ("turn.ended", Some(2)),
            ("turn.started", None),
            ("agent.tool_call", None),
            ("attention.raised", None),
        ]
    );
    let EventBody::AttentionRaised(q) = &out.events[3].body else {
        unreachable!()
    };
    assert_eq!(codex.pending_attention(), vec![q.attn_id.clone()]);
    let out = codex.respond(&q.attn_id, &Answer::new("yes")).unwrap();
    let steer: Value = serde_json::from_str(&out.replies[0]).unwrap();
    assert_eq!(steer["method"], "turn/steer");
    assert_eq!(steer["params"]["expectedTurnId"], "auto");
}

/// A held turn's request replaced (its id reused, here by a child agent's) before the answer:
/// the old item is never shown, nor withdrawn, and only the replacement stays open.
#[test]
fn codex_held_request_replaced_before_the_answer_is_never_shown() {
    for answer in [
        json!({"result":{"turn":{"id":"t2"}}}),
        json!({"error":{"code":-32000,"message":"busy"}}),
    ] {
        let mut codex = codex_open();
        let start = codex.send("next", 2);
        codex.translate(
            r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
        );
        codex.translate(r#"{"id":50,"method":"item/commandExecution/requestApproval","params":{"threadId":"parent","turnId":"auto","itemId":"rm","command":"rm -rf target"}}"#);
        let out = codex.translate(r#"{"id":50,"method":"item/commandExecution/requestApproval","params":{"threadId":"child","turnId":"child-turn","itemId":"mv","command":"mv a b"}}"#);
        let kinds: Vec<&str> = out.events.iter().map(|e| e.body.kind()).collect();
        assert_eq!(kinds, ["attention.raised"], "only the replacement: {out:?}");
        let EventBody::AttentionRaised(child) = &out.events[0].body else {
            unreachable!()
        };
        let mut answer = answer.clone();
        answer["id"] = request_id(&start[0]);
        let out = codex.translate(&answer.to_string());
        assert!(
            !out.events
                .iter()
                .any(|e| e.body.kind().starts_with("attention.")),
            "{out:?}"
        );
        assert_eq!(codex.pending_attention(), vec![child.attn_id.clone()]);
    }
}

/// Two turns Codex started on its own both complete before twr's `turn/start` is answered: each
/// counts its tokens up to its own completion, not to the latest total.
#[test]
fn codex_held_completions_count_their_own_tokens() {
    let mut codex = codex_open();
    let start = codex.send("next", 2);
    for (id, input, output) in [("a", 10, 5), ("b", 17, 8)] {
        codex.translate(
            &json!({"method":"turn/started","params":{"threadId":"parent","turn":{"id":id}}})
                .to_string(),
        );
        codex.translate(&tokens(id, input, output));
        codex.translate(&json!({"method":"turn/completed","params":{"threadId":"parent","turn":{"id":id,"status":"completed"}}}).to_string());
    }
    let out = codex.translate(
        &json!({"id":request_id(&start[0]),"error":{"code":-32000,"message":"busy"}}).to_string(),
    );
    let ends: Vec<_> = out
        .events
        .iter()
        .filter(|e| matches!(e.body, EventBody::TurnEnded(_)) && e.turn.is_none())
        .map(|e| (runtime_turn_of(&e.body), usage_of(e)))
        .collect();
    assert_eq!(
        ends,
        [(Some("a"), Some((10, 5))), (Some("b"), Some((7, 3)))],
        "17/8 in all"
    );
}

fn kinds_turns(out: &codex::Translated) -> Vec<(&str, Option<u64>)> {
    out.events.iter().map(|e| (e.body.kind(), e.turn)).collect()
}

const QUESTION: &str = r#"{"method":"item/completed","params":{"threadId":"parent","turnId":"auto","item":{"type":"agentMessage","id":"msg_q","text":"","questions":[{"title":"Keep the cache?","options":["yes","no"]}]}}}"#;
const AUTO_DONE: &str = r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"auto","status":"completed"}}}"#;

/// A turn Codex started on its own completes with a question still unanswered: Codex no
/// longer waits on it. Completed before the `turn/start` answer, the question is never shown;
/// after it, or with no answer pending at all, it is withdrawn before the turn's end, since the
/// hub may have taken that turn into twr's (whose end it does not close). twr's turn goes on.
#[test]
fn codex_own_turn_completion_closes_its_unanswered_questions() {
    // Completed before the answer.
    let mut codex = codex_open();
    let start = codex.send("next", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    codex.translate(QUESTION);
    codex.translate(AUTO_DONE);
    let out = codex
        .translate(&json!({"id":request_id(&start[0]),"result":{"turn":{"id":"t2"}}}).to_string());
    assert_eq!(
        kinds_turns(&out),
        [("turn.started", None), ("turn.ended", None)]
    );
    assert!(codex.pending_attention().is_empty());

    // Completed after it.
    let mut codex = codex_open();
    let start = codex.send("next", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    codex.translate(QUESTION);
    let out = codex
        .translate(&json!({"id":request_id(&start[0]),"result":{"turn":{"id":"t2"}}}).to_string());
    assert_eq!(
        kinds_turns(&out),
        [("turn.started", None), ("attention.raised", None)]
    );
    let out = codex.translate(AUTO_DONE);
    assert_eq!(
        kinds_turns(&out),
        [("attention.resolved", None), ("turn.ended", None)]
    );
    assert!(codex.pending_attention().is_empty());

    // No answer pending: Codex's own turn alone.
    let mut codex = codex_open();
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    codex.translate(QUESTION);
    let out = codex.translate(AUTO_DONE);
    assert_eq!(
        kinds_turns(&out),
        [("attention.resolved", None), ("turn.ended", None)]
    );
    assert!(
        matches!(&out.events[0].body, EventBody::AttentionResolved(r)
        if r.by == ResolvedBy::Timeout && r.answer.is_none())
    );
}

fn twr_error(e: &codex::Emitted) -> String {
    match &e.body {
        EventBody::TurnEnded(t) => t.ext.as_ref().unwrap()["twr"]["error"]
            .as_str()
            .unwrap()
            .to_string(),
        other => panic!("{other:?}"),
    }
}

/// An `error` notification of a turn whose owner is not known yet stays that turn's: a failed
/// turn that names no error is reported with its own last error, whichever way the
/// `turn/start` answer goes, and twr's turn keeps its own.
#[test]
fn codex_held_turn_errors_stay_their_own() {
    let error = |turn: &str, message: &str| {
        json!({"method":"error","params":{"threadId":"parent","turnId":turn,"willRetry":false,
            "error":{"message":message}}})
        .to_string()
    };
    let failed = |turn: &str| {
        json!({"method":"turn/completed","params":{"threadId":"parent","turn":{"id":turn,"status":"failed","error":null}}})
            .to_string()
    };
    let end = |out: &codex::Translated| {
        out.events
            .iter()
            .find(|e| matches!(e.body, EventBody::TurnEnded(_)))
            .cloned()
            .unwrap()
    };

    // twr's turn named: Codex's own runs beside it and keeps its cause; twr's keeps its own.
    let mut codex = codex_open();
    let start = codex.send("next", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    codex.translate(&error("auto", "auto broke"));
    codex.translate(&json!({"id":request_id(&start[0]),"result":{"turn":{"id":"t2"}}}).to_string());
    let auto = end(&codex.translate(&failed("auto")));
    assert_eq!(runtime_turn_of(&auto.body), Some("auto"));
    assert!(twr_error(&auto).contains("auto broke"), "{auto:?}");
    codex.translate(&error("t2", "t2 broke"));
    let t2 = end(&codex.translate(&failed("t2")));
    assert_eq!(t2.turn, Some(2));
    assert!(twr_error(&t2).contains("t2 broke"), "{t2:?}");

    // twr's start failed: Codex's own turn becomes the running one, with its cause.
    let mut codex = codex_open();
    let start = codex.send("next", 2);
    codex.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    codex.translate(&error("auto", "auto broke"));
    let out = codex.translate(
        &json!({"id":request_id(&start[0]),"error":{"code":-32000,"message":"busy"}}).to_string(),
    );
    assert!(twr_error(&end(&out)).contains("busy"));
    let auto = end(&codex.translate(&failed("auto")));
    assert_eq!(runtime_turn_of(&auto.body), Some("auto"));
    assert!(twr_error(&auto).contains("auto broke"), "{auto:?}");
}

/// A child agent's approval raised while the parent's turn runs belongs to the session: it
/// carries no turn and the session mark, survives the parent's turn end, and is answered after.
#[test]
fn codex_child_approval_outlives_the_parent_turn() {
    let mut tr = codex_open();
    let start = tr.send("delegate", 3);
    let id = serde_json::from_str::<Value>(&start[0]).unwrap()["id"].clone();
    tr.translate(&json!({"id":id,"result":{"turn":{"id":"turn3"}}}).to_string());
    tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","turnId":"turn3","item":{"type":"collabAgentToolCall","tool":"spawnAgent","agentsStates":{"child":{"status":"running"}}}}}"#);
    let out = tr.translate(r#"{"id":41,"method":"item/commandExecution/requestApproval","params":{"threadId":"child","turnId":"child-turn","itemId":"rm","command":"rm -rf target"}}"#);
    let (attn_id, turn, session) = out
        .events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some((
                a.attn_id.clone(),
                e.turn,
                a.ext.as_ref().unwrap()["twr"][SESSION_SCOPE_KEY].clone(),
            )),
            _ => None,
        })
        .unwrap();
    assert_eq!((turn, session), (None, json!(true)));
    let out = tr.translate(r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"turn3","status":"completed"}}}"#);
    assert!(!out
        .events
        .iter()
        .any(|e| matches!(e.body, EventBody::AttentionResolved(_))));
    assert_eq!(tr.pending_attention(), vec![attn_id.clone()]);
    let out = tr.respond(&attn_id, &Answer::new("allow")).unwrap();
    let reply: Value = serde_json::from_str(&out.replies[0]).unwrap();
    assert_eq!(reply["id"], 41);
}

fn respond_inventory(tr: &mut codex::Translator, request: &str, data: Value) -> codex::Translated {
    let id = serde_json::from_str::<Value>(request).unwrap()["id"].clone();
    tr.translate(&json!({"id":id,"result":{"data":data,"nextCursor":null}}).to_string())
}

#[test]
fn codex_yielded_shell_is_reconciled_after_turn_end() {
    let mut tr = codex_open();
    tr.send("tests", 3);
    tr.translate(r#"{"id":3,"result":{"turn":{"id":"turn3"}}}"#);
    let out = tr.translate(r#"{"method":"item/started","params":{"threadId":"parent","turnId":"turn3","item":{"type":"commandExecution","id":"c","command":"cargo test","status":"inProgress"}}}"#);
    assert_eq!(task(&out.events[0].body), Some(("shell:c", true)));
    // An empty background inventory must not retire a foreground command before it yields.
    let poll = tr.poll_background();
    assert!(respond_inventory(&mut tr, &poll[0], json!([]))
        .events
        .is_empty());
    let out = tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","turnId":"turn3","item":{"type":"commandExecution","id":"c","command":"cargo test","processId":"pid","exitCode":null,"status":"completed"}}}"#);
    assert!(!out
        .events
        .iter()
        .any(|e| task(&e.body) == Some(("shell:c", false))));
    let out = tr.translate(r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"turn3","status":"completed"}}}"#);
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e.body, EventBody::TurnEnded(_))));
    let out = respond_inventory(
        &mut tr,
        &out.replies[0],
        json!([{"itemId":"c","processId":"pid","command":"cargo test"}]),
    );
    assert!(!out
        .events
        .iter()
        .any(|e| task(&e.body) == Some(("shell:c", false))));
    let poll = tr.poll_background();
    let out = respond_inventory(&mut tr, &poll[0], json!([]));
    assert!(out
        .events
        .iter()
        .any(|e| task(&e.body) == Some(("shell:c", false))));
    assert_eq!(tr.poll_background().len(), 1);
    assert!(
        tr.poll_background().is_empty(),
        "at most one pending inventory"
    );
}

#[test]
fn codex_inventory_requested_before_yield_cannot_clear_the_yielded_command() {
    let mut tr = codex_open();
    tr.send("tests", 1);
    tr.translate(r#"{"id":3,"result":{"turn":{"id":"t"}}}"#);
    tr.translate(r#"{"method":"item/started","params":{"threadId":"parent","turnId":"t","item":{"type":"commandExecution","id":"c","command":"tests"}}}"#);
    let old = tr.poll_background();
    tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","turnId":"t","item":{"type":"commandExecution","id":"c","command":"tests","processId":"pid","exitCode":null}}}"#);
    tr.translate(r#"{"method":"turn/completed","params":{"threadId":"parent","turn":{"id":"t","status":"completed"}}}"#);
    let out = respond_inventory(&mut tr, &old[0], json!([]));
    assert!(
        out.events.is_empty(),
        "an old snapshot is not authoritative"
    );
    assert_eq!(out.replies.len(), 1, "reconcile with a fresh snapshot");
    let out = respond_inventory(
        &mut tr,
        &out.replies[0],
        json!([{"itemId":"c","command":"tests"}]),
    );
    assert!(!out
        .events
        .iter()
        .any(|e| task(&e.body) == Some(("shell:c", false))));
    let poll = tr.poll_background();
    let out = respond_inventory(&mut tr, &poll[0], json!([]));
    assert_eq!(task(&out.events[0].body), Some(("shell:c", false)));
}

#[test]
fn codex_subagent_completion_checks_its_background_shells_and_can_resume() {
    let mut tr = codex_open();
    let out = tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","item":{"type":"collabAgentToolCall","id":"spawn","tool":"spawnAgent","agentsStates":{"child":{"status":"running"}}}}}"#);
    assert_eq!(task(&out.events[0].body), Some(("agent:child", true)));
    let out = tr.translate(r#"{"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"idle"}}}"#);
    assert!(out.events.is_empty());
    let out = respond_inventory(&mut tr, &out.replies[0], json!([{"itemId":"child-shell"}]));
    assert!(!out
        .events
        .iter()
        .any(|e| task(&e.body) == Some(("agent:child", false))));
    let out = tr.translate(r#"{"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"idle"}}}"#);
    let out = respond_inventory(&mut tr, &out.replies[0], json!([]));
    assert_eq!(task(&out.events[0].body), Some(("agent:child", false)));
    let out = tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","item":{"type":"collabAgentToolCall","tool":"sendInput","agentsStates":{"child":{"status":"running"}}}}}"#);
    assert_eq!(task(&out.events[0].body), Some(("agent:child", true)));
    let out = tr.translate(
        r#"{"method":"turn/started","params":{"threadId":"parent","turn":{"id":"auto"}}}"#,
    );
    assert!(
        matches!(&out.events[0].body, EventBody::TurnStarted(t) if t.from == TurnFrom::Runtime)
    );
    assert!(tr.busy());
}

#[test]
fn codex_old_servers_reject_inventory_once_without_failing_the_session() {
    let mut tr = codex_open();
    let request: Value = serde_json::from_str(&tr.poll_background()[0]).unwrap();
    let out = tr.translate(
        &json!({"id":request["id"],"error":{"code":-32601,"message":"unknown method"}}).to_string(),
    );
    assert!(out.fatal.is_none());
    assert!(tr.poll_background().is_empty());
    assert_eq!(tr.send("still works", 1).len(), 1);
}

#[test]
fn codex_child_can_resume_itself_and_ignore_an_older_idle_snapshot() {
    let mut tr = codex_open();
    tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","item":{"type":"collabAgentToolCall","tool":"spawnAgent","agentsStates":{"child":{"status":"running"}}}}}"#);
    let idle = r#"{"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"idle"}}}"#;
    let active = r#"{"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"active"}}}"#;
    let out = tr.translate(idle);
    let old = out.replies[0].clone();
    tr.translate(active);
    assert!(respond_inventory(&mut tr, &old, json!([]))
        .events
        .is_empty());
    let out = tr.translate(idle);
    let out = respond_inventory(&mut tr, &out.replies[0], json!([]));
    assert_eq!(task(&out.events[0].body), Some(("agent:child", false)));
    let out = tr.translate(active);
    assert_eq!(task(&out.events[0].body), Some(("agent:child", true)));
    assert!(!tr.busy(), "child work is not a parent turn");
}

#[test]
fn codex_background_output_is_coalesced_progress_without_a_parent_turn() {
    let mut tr = codex_open();
    let poll = tr.poll_background();
    respond_inventory(
        &mut tr,
        &poll[0],
        json!([{"itemId":"shell","command":"tests"}]),
    );
    tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","item":{"type":"collabAgentToolCall","tool":"spawnAgent","agentsStates":{"child":{"status":"running"}}}}}"#);
    for (thread, method, item) in [
        ("parent", "item/commandExecution/outputDelta", "shell"),
        ("child", "item/agentMessage/delta", "message"),
    ] {
        let line =
            json!({"method":method,"params":{"threadId":thread,"itemId":item,"delta":"progress"}})
                .to_string();
        let out = tr.translate(&line);
        assert_eq!(out.events.len(), 1);
        assert!(
            matches!(&out.events[0].body, EventBody::TaskUpdated(t) if t.running && !t.heartbeat)
        );
        assert_eq!(out.events[0].turn, None);
        assert!(tr.translate(&line).events.is_empty());
        assert!(!tr.busy());
    }
}

#[test]
fn acp_tool_lifecycle_remains_session_scoped_after_prompt_response() {
    let mut tr = acp::Translator::with_dialect(
        Default::default(),
        acp::Standard::new(vec!["stub".into()], None, None),
    );
    tr.start();
    tr.translate(r#"{"id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#);
    tr.translate(r#"{"id":2,"result":{"sessionId":"s"}}"#);
    let out = tr.send("tests", 1);
    let request: Value = serde_json::from_str(&out.writes[0]).unwrap();
    let out = tr.translate(r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c","title":"tests","kind":"execute","status":"in_progress"}}}"#);
    assert!(out
        .events
        .iter()
        .any(|e| task(&e.body) == Some(("tool:c", true))));
    let out =
        tr.translate(&json!({"id":request["id"],"result":{"stopReason":"end_turn"}}).to_string());
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e.body, EventBody::TurnEnded(_))));
    let out = tr.translate(r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c","rawOutput":"test progress"}}}"#);
    assert!(out
        .events
        .iter()
        .any(|e| matches!(&e.body, EventBody::TaskUpdated(t) if t.running && !t.heartbeat)));
    let out = tr.translate(r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c","status":"completed"}}}"#);
    assert!(out
        .events
        .iter()
        .any(|e| task(&e.body) == Some(("tool:c", false))));
    assert!(
        !out.events
            .iter()
            .any(|e| matches!(e.body, EventBody::AgentToolCall(_))),
        "late completion must reuse the original call"
    );
}

#[test]
fn codex_child_request_withdrawn_after_the_parent_turn_is_resolved() {
    let mut tr = codex_open();
    tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","item":{"type":"collabAgentToolCall","tool":"spawnAgent","agentsStates":{"child":{"status":"running"}}}}}"#);
    let out = tr.translate(r#"{"id":40,"method":"item/commandExecution/requestApproval","params":{"threadId":"child","turnId":"child-turn","itemId":"rm","command":"rm -rf target"}}"#);
    let raised = out
        .events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some((a.attn_id.clone(), e.turn)),
            _ => None,
        })
        .unwrap();
    assert_eq!(raised.1, None, "no parent turn runs");
    let out = tr.translate(
        r#"{"method":"serverRequest/resolved","params":{"threadId":"child","requestId":40}}"#,
    );
    assert!(
        out.events.iter().any(|e| matches!(&e.body,
            EventBody::AttentionResolved(r) if r.attn_id == raised.0 && r.by == ResolvedBy::Timeout)),
        "{out:?}"
    );
    assert!(tr.pending_attention().is_empty());
}

#[test]
fn codex_active_child_read_invalidates_an_older_completion_check() {
    let mut tr = codex_open();
    tr.translate(r#"{"method":"item/completed","params":{"threadId":"parent","item":{"type":"collabAgentToolCall","tool":"spawnAgent","agentsStates":{"child":{"status":"running"}}}}}"#);
    // An idle observation starts a completion check; a later read sees the child active.
    let out = tr.translate(r#"{"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"idle"}}}"#);
    let check = out.replies[0].clone();
    let read = tr
        .poll_background()
        .into_iter()
        .find(|r| r.contains("thread/read"))
        .unwrap();
    let id = serde_json::from_str::<Value>(&read).unwrap()["id"].clone();
    tr.translate(
        &json!({"id":id,"result":{"thread":{"id":"child","status":{"type":"active"}}}}).to_string(),
    );
    // The older check's empty answer cannot retire the active child.
    assert!(respond_inventory(&mut tr, &check, json!([]))
        .events
        .is_empty());
    assert!(tr
        .poll_background()
        .iter()
        .any(|r| r.contains("thread/read")));
}

#[test]
fn acp_late_update_of_a_completed_tool_does_not_resurrect_its_task() {
    let mut tr = acp::Translator::with_dialect(
        Default::default(),
        acp::Standard::new(vec!["stub".into()], None, None),
    );
    tr.start();
    tr.translate(r#"{"id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#);
    tr.translate(r#"{"id":2,"result":{"sessionId":"s"}}"#);
    let out = tr.send("tests", 1);
    let request: Value = serde_json::from_str(&out.writes[0]).unwrap();
    tr.translate(r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c","title":"tests","kind":"execute","status":"in_progress"}}}"#);
    let out = tr.translate(r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c","status":"completed"}}}"#);
    assert!(out
        .events
        .iter()
        .any(|e| task(&e.body) == Some(("tool:c", false))));
    tr.translate(&json!({"id":request["id"],"result":{"stopReason":"end_turn"}}).to_string());
    let out = tr.translate(r#"{"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c","status":"in_progress","rawOutput":"late"}}}"#);
    assert!(
        !out.events.iter().any(|e| task(&e.body).is_some()),
        "{out:?}"
    );
}
