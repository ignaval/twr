//! The loop settings editor with ACP presets (docs/design/loops.md, "ACP reviewers"): the
//! reviewer row cycles through the presets that review, names their read-only mode, and says
//! why the others are skipped; the implementer never runs on one.

use crossterm::event::KeyCode;
use serde_json::json;

use crate::app::tests::*;
use crate::app::AppState;
use crate::loops::RowKey;
use crate::loops_tests::{change, settings_got};

fn text(buf: &ratatui::buffer::Buffer) -> String {
    let a = buf.area;
    (0..a.height)
        .map(|y| {
            (0..a.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn value(st: &AppState, key: RowKey) -> String {
    let e = st.view.loop_editor.as_ref().unwrap();
    let rt = st.loop_editor_thread_runtime();
    crate::loops::rows(e, rt.as_deref())
        .into_iter()
        .find(|r| r.key == key)
        .map(|r| r.value)
        .unwrap_or_else(|| panic!("no {key:?} row"))
}

#[test]
fn the_reviewer_row_offers_acp_presets_that_review() {
    let mut got = settings_got();
    let runtimes = got["runtimes"].as_array_mut().unwrap();
    runtimes.push(
        json!({"runtime": "ex", "found": true, "login": true, "probed": true,
        "models": ["ex-large"], "efforts": ["medium", "xhigh"], "subagents": false,
        "accounts": [], "acp": true, "review_mode": "read-only", "cannot_review": null}),
    );
    runtimes.push(
        json!({"runtime": "bare", "found": true, "login": true, "probed": true,
        "models": [], "efforts": [], "subagents": false, "accounts": [], "acp": true,
        "review_mode": null, "cannot_review": "acp preset bare names no read-only mode"}),
    );
    let mut st = three_threads();
    st.open_loop_settings(None, false);
    st.apply_result("loop.settings.get", &json!({"asked": {}, "result": got}));
    assert_eq!(value(&st, RowKey::ReviewerRuntime), "codex");
    // codex → ex (grok-build is not installed, bare can't review) → claude-code → codex.
    change(&mut st, RowKey::ReviewerRuntime, KeyCode::Right);
    assert_eq!(
        value(&st, RowKey::ReviewerRuntime),
        "ex (acp, read-only mode)"
    );
    assert_eq!(value(&st, RowKey::ReviewerModel), "ex-large");
    // (The line is cut at the editor's width.)
    let screen = text(&draw(&mut st, 140, 50));
    assert!(
        screen.contains("skipped: grok-build (not installed), bare (acp preset bare names"),
        "{screen}"
    );
    change(&mut st, RowKey::ReviewerRuntime, KeyCode::Right);
    assert_eq!(value(&st, RowKey::ReviewerRuntime), "claude-code");
    // A named implementer never runs on an ACP preset.
    change(&mut st, RowKey::Implementer, KeyCode::Right);
    for _ in 0..4 {
        change(&mut st, RowKey::ImplRuntime, KeyCode::Right);
        let rt = value(&st, RowKey::ImplRuntime);
        assert!(rt == "claude-code" || rt == "codex", "{rt}");
    }
}
