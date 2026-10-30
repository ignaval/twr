//! Captures the runner protocol fixtures of one revision of twr. Not part of the build:
//! `docs/tools/capture-runner-fixtures.sh` copies it into a temporary worktree of that
//! revision as `crates/twr-core/tests/capture_runner_fixtures.rs` and runs it there, so it
//! compiles against that revision's own `twr_core`. What it writes is what that revision
//! encodes, accepts and proves:
//!
//! - `runner.json`: one frame of each message a runner sends, and an event of each kind its
//!   runtimes emit, each as this revision encodes it;
//! - `hub-accepts.json`: the values the current hub sends a drained runner
//!   (`twr_core::testkit::outbound_samples` of the checkout, read from
//!   `TWR_CAPTURE_OUTBOUND`), each decoded by this revision and encoded back unchanged;
//! - `auth-vectors.json`: handshake proofs this revision computes, and its sealed channel's
//!   constants.
//!
//! It fails (and so the script) when this revision does not decode a value the current hub
//! sends, drops or adds anything in one, or has an event kind it does not classify.
//!
//! Environment: `TWR_CAPTURE_PROTOCOL` (the protocol the revision must speak),
//! `TWR_CAPTURE_REV` (its commit), `TWR_CAPTURE_UNCOMMITTED` (`1`: the tree captured is that
//! commit with changes on top, which each file then says, `"uncommitted": true`),
//! `TWR_CAPTURE_OUTBOUND` and `TWR_CAPTURE_OUT` (the directory to write to). Written for
//! protocols 6 and later (the challenge handshake).

use std::path::PathBuf;

use serde_json::{json, Value};
use twr_core::event::{EventBody, KNOWN_KINDS};
use twr_core::runner::{
    auth_proof, check_auth_proof, valid_nonce, CmdResult, HubMsg, Prover, RunnerMsg, Secret,
    PROTOCOL_VERSION, SEALED_AUTH_PROOF,
};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

/// Whether a runner of this revision sends events of `body`'s kind: what its runtimes'
/// adapters emit and the hub takes from them (`twr_hub::hub::supervisor::adapter_may_emit`,
/// and `runtime.limits`, which the hub folds into the account's usage), plus the runner's own
/// placeholders of those kinds. No wildcard: a revision with a kind this program does not
/// know does not compile, so the capture fails until the kind is classified here (and given a
/// sample when a runner sends it).
fn runner_emits(body: &EventBody) -> bool {
    match body {
        EventBody::ThreadStarted(_)
        | EventBody::ThreadResumed(_)
        | EventBody::TurnStarted(_)
        | EventBody::TurnEnded(_)
        | EventBody::TaskUpdated(_)
        | EventBody::AgentText(_)
        | EventBody::AgentToolCall(_)
        | EventBody::AgentToolResult(_)
        | EventBody::AgentThinking(_)
        | EventBody::AttentionRaised(_)
        | EventBody::AttentionResolved(_)
        | EventBody::RuntimeLimits(_) => true,
        EventBody::ThreadCreated(_)
        | EventBody::ThreadParked(_)
        | EventBody::ThreadStopped(_)
        | EventBody::ThreadCrashed(_)
        | EventBody::ThreadAttached(_)
        | EventBody::ThreadDetached(_)
        | EventBody::ThreadModeChanged(_)
        | EventBody::ThreadModelChanged(_)
        | EventBody::ThreadRenamed(_)
        | EventBody::ThreadDeleted(_)
        | EventBody::ThreadRestored(_)
        | EventBody::TurnWithdrawn(_)
        | EventBody::TurnInterrupted(_)
        | EventBody::PromptWithdrawn(_)
        | EventBody::ConversationUpdate(_)
        | EventBody::AttentionAnnotated(_)
        | EventBody::DecisionMade(_)
        | EventBody::ReportReady(_)
        | EventBody::GitSnapshot(_)
        | EventBody::RunnerConnected(_)
        | EventBody::RunnerDisconnected(_)
        | EventBody::ContextUpdated(_)
        | EventBody::AccountUsage(_)
        | EventBody::ProjectRead(_)
        | EventBody::WorktreeDisk(_)
        | EventBody::WorktreeTrimmed(_)
        | EventBody::Unknown { .. } => false,
    }
}

const RUN: &str = "01JZ8Q7W5M3K2N1P0R9S8T7V6X";
const NONCE_R: &str = "aa11bb22cc33dd44ee55ff6600778899aabbccddeeff00112233445566778899";

fn event(name: &str, rseq: u64, turn: Option<u64>, kind: &str, data: Value) -> (String, Value) {
    let mut f = json!({
        "type": "event",
        "thread": "t1",
        "rseq": rseq,
        "body": {"kind": kind, "data": data},
        "run": RUN,
        "at_ms": 1_792_000_000_000u64 + rseq,
    });
    if let Some(t) = turn {
        f["turn"] = json!(t);
    }
    (name.to_string(), f)
}

/// What a runner of this revision sends, as `(name, frame, since)`: frames of messages a
/// runner of protocol `since` and later sends (`agents` and `launch` came with 7).
fn runner_samples() -> Vec<(String, Value, u32)> {
    let dropped = json!({
        "bytes": 17_000_000,
        "reason": "event too large for one runner frame; its content was dropped",
    });
    let mut v: Vec<(String, Value, u32)> = vec![
        (
            "hello".into(),
            json!({"type": "hello", "box": "gpu", "protocol": PROTOCOL_VERSION, "nonce": NONCE_R}),
            6,
        ),
        (
            "auth".into(),
            json!({"type": "auth", "proof": "5f".repeat(32)}),
            6,
        ),
        (
            "auth sealed".into(),
            json!({"type": "auth", "proof": SEALED_AUTH_PROOF}),
            6,
        ),
        (
            "inventory".into(),
            json!({
                "type": "inventory",
                "version": "0.3.0+dev.0123abc",
                "epoch_hint": 2,
                "running": [
                    {"thread": "t1", "runtime": "claude-code", "session_id": "sess-1",
                     "pid": 4242, "run": RUN},
                    {"thread": "t2", "runtime": "codex", "run": "01JZ8Q7W5M3K2N1P0R9S8T7V6Y"},
                ],
                "in_flight": ["send:t1:3"],
                "acks_needed": {"t1": 9, "t2": 2},
                "runtimes": ["acp", "claude-code", "codex", "grok-build"],
                "worktrees": "/home/u/.local/share/twr/worktrees",
                "build": "0123456789abcdef",
            }),
            6,
        ),
        (
            "inventory of a fresh runner".into(),
            json!({
                "type": "inventory",
                "version": "0.3.0",
                "running": [],
                "in_flight": [],
                "acks_needed": {},
                "runtimes": ["claude-code"],
                "worktrees": "/srv/twr/worktrees",
            }),
            6,
        ),
        (
            "inventory with agents and launch".into(),
            json!({
                "type": "inventory",
                "version": "0.3.0",
                "running": [],
                "in_flight": [],
                "acks_needed": {"t1": 1},
                "runtimes": ["claude-code", "codex"],
                "worktrees": "/srv/twr/worktrees",
                "build": "fedcba9876543210",
                "agents": [
                    {"runtime": "claude-code", "found": true, "path": "/home/u/.local/bin/claude",
                     "version": "2.1.302 (Claude Code)", "login": "logged_in",
                     "method": "claude.ai"},
                    {"runtime": "codex", "found": false, "login": "unknown",
                     "note": "not found on PATH"},
                ],
                "launch": "systemd twr-box-0123456789abcdef-0a1b2c3d.service",
            }),
            7,
        ),
    ];
    let events = [
        event(
            "event thread.started",
            1,
            None,
            "thread.started",
            json!({"session_id": "sess-1", "ext": {"twr": {"pid": 4242}}}),
        ),
        event(
            "event thread.started of an acp agent",
            1,
            None,
            "thread.started",
            json!({"session_id": "acp-1",
                   "ext": {"twr": {"pid": 4300, "acp_command": ["grok", "agent", "stdio"]}}}),
        ),
        event(
            "event thread.resumed",
            1,
            None,
            "thread.resumed",
            json!({"session_id": "sess-1", "ext": {"twr": {"pid": 4243}}}),
        ),
        event(
            "event turn.started by the runtime",
            2,
            None,
            "turn.started",
            json!({"prompt": "", "from": "runtime", "routed_by": null,
                   "ext": {"twr": {"runtime_turn": "rt-1"}}}),
        ),
        event(
            "event agent.thinking",
            3,
            Some(1),
            "agent.thinking",
            json!({"text": "The tests live in crates/twr-core/tests."}),
        ),
        event(
            "event agent.text delta",
            4,
            Some(1),
            "agent.text",
            json!({"text": "Running", "delta": true, "final": false}),
        ),
        event(
            "event agent.text final",
            5,
            Some(1),
            "agent.text",
            json!({"text": "Running the tests now.", "delta": false, "final": true}),
        ),
        event(
            "event agent.tool_call",
            6,
            Some(1),
            "agent.tool_call",
            json!({"call_id": "toolu_01", "tool": "Bash", "input_summary": "cargo test",
                   "input_ref": null, "risky": true,
                   "ext": {"twr": {"purpose": "Run the tests", "step": "Testing"}}}),
        ),
        event(
            "event attention.raised permission",
            7,
            Some(1),
            "attention.raised",
            json!({"attn_id": "toolu_01", "reason": "permission",
                   "summary": "Bash: cargo test", "options": ["allow", "deny"],
                   "call_id": "toolu_01"}),
        ),
        event(
            "event attention.raised question",
            8,
            Some(1),
            "attention.raised",
            json!({"attn_id": "q-1", "reason": "question", "summary": "Which database?",
                   "options": ["Postgres", "SQLite"], "call_id": "toolu_02",
                   "ext": {"input": {"questions": [{"question": "Which database?"}]}}}),
        ),
        event(
            "event attention.raised for the session",
            9,
            None,
            "attention.raised",
            json!({"attn_id": "7:exec-1", "reason": "permission",
                   "summary": "Run `make` (child agent)", "options": ["allow", "deny"],
                   "call_id": null, "ext": {"twr": {"session": true}}}),
        ),
        event(
            "event attention.resolved by timeout",
            10,
            Some(1),
            "attention.resolved",
            json!({"attn_id": "q-1", "by": "timeout", "answer": null}),
        ),
        event(
            "event agent.tool_result",
            11,
            Some(1),
            "agent.tool_result",
            json!({"call_id": "toolu_01", "ok": true, "output_summary": "test result: ok",
                   "output_ref": null}),
        ),
        event(
            "event agent.tool_result too large for a frame",
            12,
            Some(1),
            "agent.tool_result",
            json!({"call_id": "toolu_03", "ok": false, "output_summary": "",
                   "output_ref": null, "_dropped": dropped.clone(),
                   "ext": {"twr": {"_dropped": dropped.clone()}}}),
        ),
        event(
            "event task.updated running",
            13,
            None,
            "task.updated",
            json!({"task_id": "bash-1", "running": true, "description": "cargo build",
                   "heartbeat": false}),
        ),
        event(
            "event task.updated heartbeat",
            14,
            None,
            "task.updated",
            json!({"task_id": "bash-1", "running": true, "description": "cargo build",
                   "heartbeat": true}),
        ),
        event(
            "event task.updated done",
            15,
            None,
            "task.updated",
            json!({"task_id": "bash-1", "running": false, "description": "cargo build",
                   "heartbeat": false}),
        ),
        event(
            "event runtime.limits",
            16,
            None,
            "runtime.limits",
            json!({"windows": [
                       {"window": "5h", "used_pct": 42.5, "resets_at": "2026-10-17T18:00:00Z",
                        "minutes": 300},
                       {"window": "7d", "used_pct": 3.0}],
                   "source": "claude-code", "plan": "max"}),
        ),
        event(
            "event turn.ended completed",
            17,
            Some(1),
            "turn.ended",
            json!({"result": "completed", "session_id": "sess-1",
                   "usage": {"input": 1200, "output": 340, "cost": 0.0123},
                   "duration_ms": 5300}),
        ),
        event(
            "event turn.ended error",
            18,
            Some(2),
            "turn.ended",
            json!({"result": "error", "session_id": "sess-1", "usage": null,
                   "duration_ms": 120, "ext": {"error": "overloaded"}}),
        ),
        event(
            "event turn.ended interrupted",
            19,
            Some(3),
            "turn.ended",
            json!({"result": "interrupted", "session_id": null, "usage": null,
                   "duration_ms": null}),
        ),
        event(
            "event turn.ended of a runtime turn",
            20,
            None,
            "turn.ended",
            json!({"result": "completed", "session_id": "sess-1", "usage": null,
                   "duration_ms": 900, "ext": {"twr": {"runtime_turn": "rt-1"}}}),
        ),
        event(
            "event turn.ended too large for a frame",
            21,
            Some(4),
            "turn.ended",
            json!({"result": "completed", "session_id": "sess-1", "usage": null,
                   "duration_ms": null, "ext": {"twr": {"_dropped": dropped}}}),
        ),
    ];
    v.extend(events.into_iter().map(|(n, f)| (n, f, 6)));
    let mut raw = event(
        "event agent.text with its raw line",
        22,
        Some(1),
        "agent.text",
        json!({"text": "done", "delta": false, "final": true}),
    );
    raw.1["raw"] = json!(r#"{"type":"assistant","message":{"content":"done"}}"#);
    let mut bare = event(
        "event without run or time",
        23,
        Some(1),
        "agent.thinking",
        json!({"text": "hm"}),
    );
    bare.1.as_object_mut().unwrap().remove("run");
    bare.1.as_object_mut().unwrap().remove("at_ms");
    v.push((raw.0, raw.1, 6));
    v.push((bare.0, bare.1, 6));
    let exits = [
        (
            "exited",
            json!({"type": "exited", "thread": "t1", "rseq": 24, "code": 0, "stderr_tail": "",
                   "run": RUN, "at_ms": 1_792_000_000_100u64}),
        ),
        (
            "exited at a shutdown",
            json!({"type": "exited", "thread": "t1", "rseq": 24, "code": null,
                   "stderr_tail": "twr runner: shut down before the runtime exited",
                   "run": RUN, "at_ms": 1_792_000_000_100u64}),
        ),
        (
            "exited of a runtime a previous runner left behind",
            json!({"type": "exited", "thread": "t2", "rseq": 3, "code": null,
                   "stderr_tail": "twr runner: the runner that started this runtime died \
                                   without recording its exit; it was stopped",
                   "at_ms": 1_792_000_000_200u64}),
        ),
        (
            "exited with a full spool",
            json!({"type": "exited", "thread": "t1", "rseq": 25, "code": null,
                   "stderr_tail": "twr runner: spool full (268435456 bytes); runtime stopped, \
                                   later events lost",
                   "run": RUN}),
        ),
        ("synced", json!({"type": "synced"})),
        (
            "cmd_result ok",
            serde_json::to_value(RunnerMsg::CmdResult(CmdResult::new(
                "send:t1:3",
                Ok(Value::Null),
            )))
            .unwrap(),
        ),
        (
            "cmd_result ok with a value",
            json!({"type": "cmd_result", "cmd_id": "01JZ8Q7W5M3K2N1P0R9S8T7V6Z", "ok": true,
                   "result": {"branch": "twr/x", "dirty_files": 1}}),
        ),
        (
            "cmd_result error",
            json!({"type": "cmd_result", "cmd_id": "stop:t1:2", "ok": false,
                   "error": {"code": "not_running", "message": "thread t1 has no live session"}}),
        ),
        (
            "cmd_result interrupted",
            json!({"type": "cmd_result", "cmd_id": "respond:2:01JZ8Q7W5M3K2N1P0R9S8T7V6X",
                   "ok": false,
                   "error": {"code": "interrupted",
                             "message": "the runner stopped while the command ran; its effect \
                                         is unknown"}}),
        ),
        ("ping", json!({"type": "ping"})),
        ("pong", json!({"type": "pong"})),
    ];
    v.extend(exits.into_iter().map(|(n, f)| (n.to_string(), f, 6)));
    v
}

/// `frame` with a `cmd_result`'s `"result": null` taken out: a runner sends it for an op
/// that answers nothing, and it decodes as no result (the same null outcome), so it does not
/// come back from a round trip.
fn without_null_result(frame: &Value) -> Value {
    let mut f = frame.clone();
    if f["type"] == "cmd_result" && f.get("result").is_some_and(Value::is_null) {
        f.as_object_mut().unwrap().remove("result");
    }
    f
}

/// Decodes `frame` as this revision's `T` and encodes it back: it must come back unchanged
/// (nothing dropped, nothing added; see [`without_null_result`]).
fn same_after_round_trip<T>(what: &str, name: &str, frame: &Value) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let m: T = serde_json::from_value(frame.clone())
        .unwrap_or_else(|e| panic!("{what} {name:?} does not decode at this revision: {e}"));
    let back = serde_json::to_value(&m).unwrap();
    assert_eq!(
        without_null_result(&back),
        without_null_result(frame),
        "{what} {name:?} changes when this revision decodes and encodes it"
    );
    m
}

fn write(dir: &std::path::Path, file: &str, v: &Value) {
    let mut v = v.clone();
    if std::env::var("TWR_CAPTURE_UNCOMMITTED").is_ok_and(|u| u == "1") {
        v["uncommitted"] = Value::Bool(true);
    }
    let mut s = serde_json::to_string_pretty(&v).unwrap();
    s.push('\n');
    std::fs::write(dir.join(file), s).unwrap();
}

#[test]
fn capture() {
    let protocol: u32 = var("TWR_CAPTURE_PROTOCOL").parse().unwrap();
    let rev = var("TWR_CAPTURE_REV");
    let out = PathBuf::from(var("TWR_CAPTURE_OUT"));
    assert_eq!(
        PROTOCOL_VERSION, protocol,
        "this revision speaks runner protocol {PROTOCOL_VERSION}, not {protocol}"
    );
    std::fs::create_dir_all(&out).unwrap();

    // What its runner sends.
    let mut samples = Vec::new();
    let mut sampled_kinds = std::collections::BTreeSet::new();
    for (name, frame, since) in runner_samples() {
        if protocol < since {
            continue;
        }
        let m: RunnerMsg = same_after_round_trip("runner frame", &name, &frame);
        let mut entry = json!({"name": name, "frame": frame});
        if let RunnerMsg::Event { body, .. } = &m {
            assert!(
                !matches!(body, EventBody::Unknown { .. }) && runner_emits(body),
                "{name:?}: {} is no kind a runner of this revision emits",
                body.kind()
            );
            sampled_kinds.insert(body.kind().to_string());
            entry["event_kind"] = json!(body.kind());
        }
        samples.push(entry);
    }
    let mut emitted = Vec::new();
    for kind in KNOWN_KINDS {
        let body = EventBody::from_kind_data(kind, Value::Null).unwrap();
        if runner_emits(&body) {
            assert!(
                sampled_kinds.contains(*kind),
                "a runner of this revision emits {kind}, and no sample has it"
            );
            emitted.push(*kind);
        }
    }
    write(
        &out,
        "runner.json",
        &json!({
            "protocol": protocol,
            "revision": rev,
            "what": "frames a runner of this revision sends, as it encodes them",
            "emitted_kinds": emitted,
            "samples": samples,
        }),
    );

    // What it accepts of the current hub's values.
    let outbound: Value =
        serde_json::from_str(&std::fs::read_to_string(var("TWR_CAPTURE_OUTBOUND")).unwrap())
            .unwrap();
    let mut accepted = Vec::new();
    for s in outbound["samples"].as_array().expect("samples") {
        let name = s["name"].as_str().unwrap();
        let frame = &s["frame"];
        let m: HubMsg = same_after_round_trip("hub value", name, frame);
        match &m {
            HubMsg::Challenge { nonce, proof } => {
                assert!(
                    valid_nonce(nonce),
                    "{name:?}: this revision refuses its nonce"
                );
                assert!(
                    proof.len() == 64 && proof.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{name:?}: not a proof this revision can check"
                );
            }
            HubMsg::Cmd { op, .. } => {
                assert_eq!(op.name(), frame["op"]["type"], "{name:?}");
                assert!(
                    op.is_session_op(),
                    "{name:?}: this revision does not run {} in its session's order",
                    op.name()
                );
            }
            _ => {}
        }
        accepted.push(json!({"name": name, "frame": frame}));
    }
    write(
        &out,
        "hub-accepts.json",
        &json!({
            "protocol": protocol,
            "revision": rev,
            "what": "values the hub sends a drained runner, each decoded and encoded back \
                     unchanged by this revision",
            "accepted": accepted,
        }),
    );

    // Its proofs, and its sealed channel.
    let mut vectors = Vec::new();
    for (token, box_name, hub_nonce) in [
        (
            "0123456789abcdef0123456789abcdef",
            "gpu",
            "0f1e2d3c4b5a69788796a5b4c3d2e1f000112233445566778899aabbccddeeff",
        ),
        (
            "s3cr3t-T0ken_with.punctuation~0123456789",
            "mac-mini.local",
            "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100",
        ),
    ] {
        for (by, label) in [(Prover::Hub, "hub"), (Prover::Runner, "runner")] {
            let t = Secret::new(token);
            let proof = auth_proof(&t, by, box_name, NONCE_R, hub_nonce);
            assert!(check_auth_proof(
                &t, by, box_name, NONCE_R, hub_nonce, &proof
            ));
            vectors.push(json!({
                "connection": "token", "by": label, "box": box_name, "token": token,
                "runner_nonce": NONCE_R, "hub_nonce": hub_nonce, "proof": proof,
            }));
        }
    }
    vectors.push(json!({
        "connection": "sealed", "by": "runner", "proof": SEALED_AUTH_PROOF,
        "note": "on a sealed connection the runner sends this proof and does not check the \
                 hub's; the hub does not check the runner's but that it is this one",
    }));
    write(
        &out,
        "auth-vectors.json",
        &json!({
            "protocol": protocol,
            "revision": rev,
            "what": "handshake proofs this revision computes, and its sealed channel",
            "vectors": vectors,
            "sealed": {
                "channel_version": twr_core::sealed::CHANNEL_VERSION,
                "hello_prefix": twr_core::sealed::HELLO_PREFIX,
                "key_prefix": twr_core::sealed::KEY_PREFIX,
            },
        }),
    );
}
