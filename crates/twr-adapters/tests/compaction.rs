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
//!
//! How full Claude's context is (`turn.ended` `context`: the main agent's last call, not the
//! turn's summed usage) is checked on a recording and on lines written here.

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

// --- How full Claude's context is -------------------------------------------------------------

/// The `ext.claude.context_from` of a turn end.
fn context_from(e: &TurnEnded) -> Option<&str> {
    e.ext.as_ref()?.get("claude")?.get("context_from")?.as_str()
}

/// A recorded turn of two model calls: its `result` `usage` sums both (what it spent), its
/// context is the second call's input alone (8 + 1,849 + 23,010), as Claude's own
/// `get_context_usage` counts it.
#[test]
fn claude_recorded_two_call_turn_reports_the_last_call() {
    let events = claude("resume_part1");
    let bodies = claude_bodies(&events);
    let ends = ended(&bodies);
    assert_eq!(ends.len(), 1);
    let usage = ends[0].usage.as_ref().unwrap();
    assert_eq!(usage.input, 18 + 6_144 + 41_725);
    assert_eq!(
        ends[0].context,
        Some(ContextSize {
            tokens: 8 + 1_849 + 23_010,
            window: Some(200_000)
        })
    );
    assert_eq!(context_from(&ends[0]), Some("last_call"));
}

/// One Claude model call of the main agent (`parent` none) or a subagent's, as `stream-json`
/// sends it: one `assistant` line per content block, each with the call's `usage`.
fn call_lines(id: &str, model: &str, parent: Option<&str>, input: [u64; 3]) -> Vec<String> {
    let usage = json!({"input_tokens": input[0], "cache_creation_input_tokens": input[1],
        "cache_read_input_tokens": input[2], "output_tokens": 7});
    ["thinking", "text"]
        .iter()
        .map(|kind| {
            json!({"type": "assistant", "parent_tool_use_id": parent, "session_id": "s",
                "message": {"id": id, "model": model, "role": "assistant", "usage": usage,
                    "content": [{"type": kind, "thinking": ".", "text": "."}]}})
            .to_string()
        })
        .collect()
}

fn result_line(num_turns: u64, input: [u64; 3], model_usage: Value) -> String {
    json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": num_turns,
        "stop_reason": "end_turn", "session_id": "s", "total_cost_usd": 1.0,
        "usage": {"input_tokens": input[0], "cache_creation_input_tokens": input[1],
            "cache_read_input_tokens": input[2], "output_tokens": 70},
        "modelUsage": model_usage})
    .to_string()
}

const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"s","model":"claude-opus-5-5","permissionMode":"default"}"#;

/// The turn of the 693% card, in small: eight calls of the main agent whose inputs add up to
/// 1.2M tokens of a 1M window, while the last of them held 180k. The context is that last
/// call's input, and the window its model's (a subagent's model has a smaller one); the turn's
/// spend stays the sum. A subagent's call, larger than the main agent's, a `<synthetic>`
/// message reporting zeros and the `message_start` of the last call change nothing. Then a
/// manual compaction brings it down to what the compaction left, the next call up again, and a
/// turn whose calls report no size leaves it unknown rather than reading the turn's sum.
#[test]
fn claude_context_is_the_latest_calls_input_not_the_turns_spend() {
    let mut tr = claude_code::Translator::new(claude_code::TranslatorOptions::default());
    let mut ends = Vec::new();
    macro_rules! feed {
        ($line:expr $(,)?) => {
            for e in tr.translate($line).events {
                if let EventBody::TurnEnded(t) = e.body {
                    ends.push(t);
                }
            }
        };
    }
    let models = json!({
        "claude-opus-5-5": {"inputTokens": 16, "contextWindow": 1_000_000},
        "claude-haiku-4-5-20251001": {"inputTokens": 190_000, "contextWindow": 200_000},
    });

    tr.user_message("research", 1);
    feed!(INIT);
    let mut spent = [0u64; 3];
    for i in 0..8u64 {
        let input = [2, 1_000, 109_000 + i * 10_000];
        if i == 7 {
            let start = json!({"type": "stream_event", "parent_tool_use_id": null,
                "event": {"type": "message_start", "message": {"id": "m7",
                    "model": "claude-opus-5-5", "usage": {"input_tokens": 2,
                        "cache_creation_input_tokens": 1_000, "cache_read_input_tokens": 179_000,
                        "output_tokens": 1}}}});
            feed!(&start.to_string());
        }
        for line in call_lines(&format!("m{i}"), "claude-opus-5-5", None, input) {
            feed!(&line);
        }
        if i == 3 {
            let sub = call_lines(
                "sub",
                "claude-haiku-4-5-20251001",
                Some("toolu_1"),
                [190_000, 0, 0],
            );
            for line in sub {
                feed!(&line);
            }
        }
        for (s, n) in spent.iter_mut().zip(input) {
            *s += n;
        }
    }
    let synthetic = json!({"type": "assistant", "parent_tool_use_id": null, "message": {
        "id": "x", "model": "<synthetic>", "content": [{"type": "text", "text": "."}],
        "usage": {"input_tokens": 0, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0, "output_tokens": 0}}});
    feed!(&synthetic.to_string());
    feed!(&result_line(8, spent, models.clone()));

    let first = &ends[0];
    let total: u64 = spent.iter().sum();
    assert!(
        total > 1_000_000,
        "the turn's input exceeds the window: {total}"
    );
    assert_eq!(first.usage.as_ref().unwrap().input, total);
    let context = first.context.expect("the last call's size");
    assert_eq!(
        context,
        ContextSize {
            tokens: 2 + 1_000 + 179_000,
            window: Some(1_000_000)
        }
    );
    assert_eq!(context.percent(), Some(18));
    assert_eq!(context_from(first), Some("last_call"));

    // A manual compaction: no call in its turn; the context is what it left.
    tr.compact_message(2);
    feed!(r#"{"type":"system","subtype":"status","status":"compacting","session_id":"s"}"#,);
    feed!(INIT);
    feed!(
        r#"{"type":"system","subtype":"compact_boundary","session_id":"s","compact_metadata":{"trigger":"manual","pre_tokens":182002,"post_tokens":40000}}"#,
    );
    feed!(&result_line(0, [0, 0, 0], models.clone()));
    assert_eq!(
        ends[1].context,
        Some(ContextSize {
            tokens: 40_000,
            window: Some(1_000_000)
        })
    );
    assert_eq!(context_from(&ends[1]), Some("compaction"));

    // The next call reads the context again.
    tr.user_message("more", 3);
    feed!(INIT);
    for line in call_lines("m8", "claude-opus-5-5", None, [3, 2_000, 40_000]) {
        feed!(&line);
    }
    feed!(&result_line(1, [3, 2_000, 40_000], models.clone()));
    assert_eq!(ends[2].context.map(|c| c.tokens), Some(42_003));

    // Calls without `usage`: their turn's sum is no context size, and none is said.
    let bare = |id: &str| {
        json!({"type": "assistant", "parent_tool_use_id": null, "message": {
            "id": id, "model": "claude-opus-5-5", "content": [{"type": "text", "text": "."}]}})
        .to_string()
    };
    tr.user_message("again", 4);
    feed!(INIT);
    feed!(&bare("m9"));
    feed!(&bare("m10"));
    feed!(&result_line(2, [5, 1_000, 90_000], models.clone()));
    assert_eq!(ends[3].context, None);
    assert_eq!(context_from(&ends[3]), Some("unknown"));
    assert_eq!(ends[3].usage.as_ref().unwrap().input, 91_005);

    // A call with its size, then one without: not the first one's size. Nor when the `result`
    // reports no usage at all (an interrupted turn's are zeros).
    tr.user_message("once more", 5);
    feed!(INIT);
    for line in call_lines("m11", "claude-opus-5-5", None, [3, 2_000, 60_000]) {
        feed!(&line);
    }
    feed!(&bare("m12"));
    feed!(&result_line(2, [0, 0, 0], models.clone()));
    assert_eq!(ends[4].context, None);
    assert_eq!(context_from(&ends[4]), Some("unknown"));

    // A compaction's size is its turn's: a later turn showing no call reads its own `result`
    // (one call: that call's input), not what the compaction left.
    let compact = |tr: &mut claude_code::Translator, turn: u64, post: &str| {
        tr.compact_message(turn);
        let boundary = format!(
            r#"{{"type":"system","subtype":"compact_boundary","session_id":"s","compact_metadata":{{"trigger":"manual","pre_tokens":90000{post}}}}}"#
        );
        [
            r#"{"type":"system","subtype":"status","status":"compacting","session_id":"s"}"#
                .to_string(),
            INIT.to_string(),
            boundary,
        ]
    };
    for line in compact(&mut tr, 6, r#","post_tokens":30000"#) {
        feed!(&line);
    }
    feed!(&result_line(0, [0, 0, 0], models.clone()));
    assert_eq!(ends[5].context.map(|c| c.tokens), Some(30_000));
    tr.user_message("result only", 7);
    feed!(INIT);
    feed!(&result_line(1, [5, 0, 80_000], models.clone()));
    assert_eq!(ends[6].context.map(|c| c.tokens), Some(80_005));
    assert_eq!(context_from(&ends[6]), Some("last_call"));
    // A compaction that does not say what it left: unknown, not the size before it; and a
    // later turn of several calls showing none is unknown too.
    for line in compact(&mut tr, 8, "") {
        feed!(&line);
    }
    feed!(&result_line(0, [0, 0, 0], models.clone()));
    assert_eq!(ends[7].context, None);
    assert_eq!(context_from(&ends[7]), Some("unknown"));
    tr.user_message("several", 9);
    feed!(INIT);
    feed!(&result_line(2, [5, 0, 100_000], models));
    assert_eq!(ends[8].context, None);
    assert_eq!(context_from(&ends[8]), Some("unknown"));
}

/// The window is the main agent's model's, not the largest of the turn: a main agent on a
/// 200k model with a subagent on a 1M one reads against 200k. With no window said for the main
/// agent's model, the window is unknown (the size alone), even when the other models share one,
/// and so it is when the call is larger than the window said.
#[test]
fn claude_context_window_is_the_main_agents_models() {
    let run = |model: &str, model_usage: Value, input: [u64; 3]| {
        let mut tr = claude_code::Translator::new(claude_code::TranslatorOptions::default());
        tr.user_message("go", 1);
        let mut lines = vec![INIT.to_string()];
        lines.extend(call_lines("m1", model, None, input));
        lines.extend(call_lines(
            "s1",
            "claude-opus-5-5",
            Some("toolu_1"),
            [9, 0, 300_000],
        ));
        lines.push(result_line(1, input, model_usage));
        lines
            .iter()
            .flat_map(|l| tr.translate(l).events)
            .find_map(|e| match e.body {
                EventBody::TurnEnded(t) => Some(t.context),
                _ => None,
            })
            .unwrap()
    };
    let both = json!({
        "claude-haiku-4-5-20251001": {"contextWindow": 200_000},
        "claude-opus-5-5[1m]": {"contextWindow": 1_000_000},
    });
    let ctx = |tokens, window| Some(ContextSize { tokens, window });
    assert_eq!(
        run("claude-haiku-4-5-20251001", both.clone(), [10, 0, 50_000]),
        ctx(50_010, Some(200_000))
    );
    // `[1m]` on the key, not on the call's model.
    assert_eq!(
        run("claude-opus-5-5", both.clone(), [10, 0, 50_000]),
        ctx(50_010, Some(1_000_000))
    );
    // By `canonicalModel`.
    let canonical = json!({
        "opus": {"contextWindow": 1_000_000, "canonicalModel": "claude-opus-5-5"},
        "claude-haiku-4-5-20251001": {"contextWindow": 200_000},
    });
    assert_eq!(
        run("claude-opus-5-5", canonical, [10, 0, 50_000]),
        ctx(50_010, Some(1_000_000))
    );
    assert_eq!(
        run("claude-sonnet-9", both, [10, 0, 50_000]),
        ctx(50_010, None)
    );
    let other = json!({"claude-haiku-4-5-20251001": {"contextWindow": 200_000}});
    assert_eq!(
        run("claude-opus-5-5", other, [10, 0, 50_000]),
        ctx(50_010, None)
    );
    let small = json!({"claude-opus-5-5": {"contextWindow": 200_000}});
    assert_eq!(
        run("claude-opus-5-5", small, [10, 0, 250_000]),
        ctx(250_010, None)
    );
    // `m` and `m[1m]` disagree: which one ran is not said.
    let ambiguous = json!({
        "claude-opus-5-5": {"contextWindow": 200_000},
        "claude-opus-5-5[1m]": {"contextWindow": 1_000_000},
    });
    assert_eq!(
        run("claude-opus-5-5", ambiguous, [10, 0, 50_000]),
        ctx(50_010, None)
    );
    // No model said by the call: the window every entry says, not one some leave out.
    let partial = json!({"main": {}, "child": {"contextWindow": 1_000_000}});
    assert_eq!(run("", partial, [10, 0, 50_000]), ctx(50_010, None));
    let shared =
        json!({"main": {"contextWindow": 1_000_000}, "child": {"contextWindow": 1_000_000}});
    assert_eq!(
        run("", shared, [10, 0, 50_000]),
        ctx(50_010, Some(1_000_000))
    );
    // No `modelUsage` at all: the size without a window.
    assert_eq!(
        run("claude-opus-5-5", Value::Null, [10, 0, 50_000]),
        ctx(50_010, None)
    );
}

/// One translator across turns: a window it knew of the main agent's model does not stand in
/// when the turn's entries of that model disagree (`m` and `m[1m]`); and a call that does not
/// say its model is not the previous call's model.
#[test]
fn claude_context_window_across_turns() {
    let mut tr = claude_code::Translator::new(claude_code::TranslatorOptions::default());
    let mut turn = 0;
    let mut run = |calls: &[(&str, &str)], model_usage: Value| {
        turn += 1;
        tr.user_message("go", turn);
        let mut lines = vec![INIT.to_string()];
        for (id, model) in calls {
            lines.extend(call_lines(id, model, None, [10, 0, 150_000]));
        }
        lines.push(result_line(
            calls.len() as u64,
            [10, 0, 150_000],
            model_usage,
        ));
        lines
            .iter()
            .flat_map(|l| tr.translate(l).events)
            .find_map(|e| match e.body {
                EventBody::TurnEnded(t) => Some(t.context.and_then(|c| c.window)),
                _ => None,
            })
            .unwrap()
    };
    let m = "claude-opus-5-5";
    assert_eq!(
        run(&[("a", m)], json!({ m: {"contextWindow": 200_000} })),
        Some(200_000)
    );
    // Nothing said of the model: the window known.
    assert_eq!(run(&[("b", m)], json!({})), Some(200_000));
    // Entries that disagree: unknown, and not known after either.
    let both = json!({ m: {"contextWindow": 200_000}, "claude-opus-5-5[1m]": {"contextWindow": 1_000_000} });
    assert_eq!(run(&[("c", m)], both), None);
    assert_eq!(run(&[("d", m)], json!({})), None);
    // A later call that names no model: the turn's models' windows disagree, so unknown.
    let two =
        json!({ m: {"contextWindow": 200_000}, "claude-haiku-5-5": {"contextWindow": 1_000_000} });
    assert_eq!(run(&[("e", m), ("f", "")], two.clone()), None);
    assert_eq!(run(&[("g", m)], two), Some(200_000));
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
