//! Implementation loops in the TUI ([`crate::loops`]): `/implement` and `/new … --implement`
//! to the hub's requests and both of its answers, `/loop` that starts none, the popup's
//! `implementation loop…`, the setup card that holds a task (three options), the editor's
//! plan check and commit rows, the loop line of every phase at several widths, the popup's
//! progress and rows per phase, the dive's folded rows of a whole loop, and how a loop ended.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::text::Line;
use serde_json::{json, Value};
use twr_core::event::{
    AttentionRaised, AttentionReason, EventBody, TaskUpdated, LOOP_SETUP_KEY, LOOP_STUCK_KEY,
};
use twr_core::loops::{LoopKind, LoopSetup};
use twr_core::testkit::ThreadScript;

use super::*;
use crate::app::tests::*;
use crate::app::{Action, ActionKind, AppState, Transcript, CARD_SETTLE};
use crate::loops::RowKey;
use crate::loops_tests::{change, hub, impl_started, implementing, settings_got, ImplRun, Upto};

fn lines(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (0..a.height)
        .map(|y| (0..a.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn text(buf: &Buffer) -> String {
    lines(buf).join("\n")
}

fn flat(rows: &[Line<'static>]) -> Vec<String> {
    rows.iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// Draws at 160 x 40, notes it, and pretends it has been on screen long enough to read.
fn read(st: &mut AppState) -> String {
    let buf = draw(st, 160, 40);
    st.note_drawn(Instant::now());
    if let Some(s) = st.view.shown.as_mut() {
        s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
    }
    text(&buf)
}

fn kinds(st: &AppState) -> Vec<ActionKind> {
    let target = st.view.action.as_ref().unwrap().target.clone();
    st.action_rows(&target)
        .into_iter()
        .map(|r| r.kind)
        .collect()
}

fn loop_call(actions: &[Action]) -> Vec<(&'static str, Value)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Loop { method, params } => Some((*method, params.clone())),
            _ => None,
        })
        .collect()
}

/// An implementation loop's setup card, as the hub writes it, holding a task.
pub(crate) const IMPL_SUMMARY: &str = "gamma: implementation loop setup (first run: choose its \
models). Start and save these settings?
Task         add a CSV export to the reports page, with a header row, one row per report and \
the totals last, and a test that reads it back
Profile      medium · medium tier · sweep · 2 verify
Implementer  fake · its default model (the thread's own)
Workers      claude-opus-5-5 · high
Reviewer     codex · gpt-6.1-sol · medium → xhigh
Plan check   contracts
Commit       end
Save for     all projects";

/// Raises an implementation loop's setup card (`ls2`) on `t-gamma` holding a task: its three
/// options.
pub(crate) fn raise_impl_setup(st: &mut AppState, summary: &str) {
    let setup = LoopSetup {
        kind: LoopKind::Implement,
        first_run: true,
        project: Some("proj".into()),
        task: Some("add a CSV export".into()),
        ..Default::default()
    };
    let mut g = ThreadScript::new("t-gamma");
    g.at(600);
    let ext = json!({ "twr": { LOOP_SETUP_KEY: setup } });
    let ev = hub(
        &mut g,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "ls2".into(),
            reason: AttentionReason::Review,
            summary: summary.into(),
            options: vec![
                twr_core::event::LOOP_SETUP_START.into(),
                twr_core::event::LOOP_SETUP_RUN_WITHOUT.into(),
                twr_core::event::LOOP_SETUP_CANCEL.into(),
            ],
            ext: ext.as_object().cloned(),
            ..Default::default()
        }),
    );
    control(st, &[ev]);
}

// ---------------------------------------------------------------------------------------------
// /implement, /new … --implement, /loop
// ---------------------------------------------------------------------------------------------

/// `/implement`: a first word naming a live thread is the thread, else the selected card's;
/// in a dive every word is the task. The flags go as this run's overrides, `--plan-check`
/// among them; no task sends none.
#[test]
fn implement_requests_an_implementation_loop() {
    let mut st = three_threads();
    st.view.width = 160;
    assert_eq!(
        loop_call(&type_line(
            &mut st,
            "/implement gamma --setup --plan-check always --commit per-round add a CSV export"
        )),
        [(
            "loop.request",
            json!({"thread": "t-gamma", "kind": "implement", "task": "add a CSV export",
                   "setup": true,
                   "overrides": {"plan_check": "always", "commit": "per-round"}})
        )]
    );
    assert_eq!(
        st.view.status.as_deref(),
        Some("implementation loop on gamma: asking the hub")
    );
    // No thread named: the selected card's, every word the task.
    st.set_focus(Some("t-beta".into()));
    assert_eq!(
        loop_call(&type_line(
            &mut st,
            "/implement add rate limiting --profile low --reviewer codex/gpt-6.1-sol"
        )),
        [(
            "loop.request",
            json!({"thread": "t-beta", "kind": "implement", "task": "add rate limiting",
                   "overrides": {"profile": "low", "reviewer": "codex/gpt-6.1-sol"}})
        )]
    );
    // A thread named and no task: the conversation settled it.
    assert_eq!(
        loop_call(&type_line(&mut st, "/implement alpha")),
        [(
            "loop.request",
            json!({"thread": "t-alpha", "kind": "implement"})
        )]
    );
    // In a dive, a first word that names a thread is the task's.
    st.dive("t-alpha");
    assert_eq!(
        loop_call(&type_line(&mut st, "/implement gamma export -- --raw too")),
        [(
            "loop.request",
            json!({"thread": "t-alpha", "kind": "implement",
                   "task": "gamma export --raw too"})
        )]
    );
    assert_eq!(
        loop_call(&type_line(&mut st, "/implement beta first")),
        [(
            "loop.request",
            json!({"thread": "t-alpha", "kind": "implement", "task": "beta first"})
        )]
    );
    // `--scope` is a review loop's: the parser says so, nothing is sent.
    assert!(loop_call(&type_line(&mut st, "/implement --scope branch x")).is_empty());
    assert!(
        st.view.status.as_deref().unwrap().contains("has no scope"),
        "{:?}",
        st.view.status
    );
}

/// With no card selected, no dive and no thread named, `/implement` says there is no thread to
/// run it in and how to name one; the orchestrator runs none.
#[test]
fn implement_with_no_thread_says_how_to_name_one() {
    let mut st = three_threads();
    st.view.width = 160;
    st.set_focus(None);
    assert!(loop_call(&type_line(&mut st, "/implement add a CSV export")).is_empty());
    assert_eq!(
        st.view.status.as_deref(),
        Some(
            "no thread to run it in: select a card, dive into a thread, or name one first \
             (/implement <thread> <task>)"
        )
    );
}

/// Both of the hub's answers, as `/review`'s: `started` says what it runs on; `needs_setup`
/// leaves the dive and selects the thread's card, which nothing answers; an error says so.
#[test]
fn implement_answers_started_needs_setup_and_errors() {
    let mut st = three_threads();
    st.view.width = 160;
    st.dive("t-gamma");
    let asked = json!({"thread": "t-gamma", "kind": "implement", "task": "add it"});
    st.apply_result(
        "loop.request",
        &json!({"asked": asked,
                "result": {"status": "needs_setup", "attn_id": "ls2", "revision": 2,
                           "why": "first_run"}}),
    );
    assert_eq!(st.view.dive, None);
    assert_eq!(st.focused(), Some("t-gamma"));
    assert_eq!(
        st.view.status.as_deref(),
        Some(
            "gamma's implementation loop waits for its setup card: read it on gamma's card \
             (Enter opens it); 1 starts the loop and saves the settings"
        )
    );
    st.apply_result(
        "loop.request",
        &json!({"asked": asked,
                "result": {"status": "started",
                           "loop": {"line": "medium · plan",
                                    "reviewer": {"runtime": "codex", "model": "gpt-6.1-sol"}}}}),
    );
    assert_eq!(
        st.view.status.as_deref(),
        Some("gamma: implementation loop started (medium · plan); reviewer codex · gpt-6.1-sol")
    );
    st.apply_result(
        "loop.request",
        &json!({"asked": asked, "failed": true, "code": "conflict",
                "error": "gamma runs a review loop already"}),
    );
    assert_eq!(
        st.view.status.as_deref(),
        Some("implementation loop: gamma runs a review loop already")
    );
}

/// `/new <project> --implement [--setup] <task>`: `thread.new` with the task as its first
/// prompt and `loop {kind: implement, setup?}`, with `/new`'s other options as they were.
#[test]
fn new_with_implement_starts_a_thread_on_its_task() {
    let mut st = three_threads();
    st.view.width = 160;
    let acts = type_line(
        &mut st,
        "/new api --runtime codex --implement --setup add rate limiting to the webhooks",
    );
    assert_eq!(
        acts,
        [Action::NewThread {
            project: "api".into(),
            runtime: Some("codex".into()),
            name: None,
            account: None,
            outside_pin: false,
            implement: Some(crate::app::NewLoop {
                task: "add rate limiting to the webhooks".into(),
                setup: true,
            }),
        }]
    );
    assert_eq!(
        crate::action_request(&acts[0]),
        Some((
            "thread.new",
            json!({"project": "api", "runtime": "codex",
                   "loop": {"kind": "implement", "task": "add rate limiting to the webhooks",
                            "setup": true}})
        ))
    );
    assert_eq!(
        st.view.status.as_deref(),
        Some("creating a thread in api for an implementation loop...")
    );
    let acts = type_line(&mut st, "/new api --implement add it");
    assert_eq!(
        crate::action_request(&acts[0]),
        Some((
            "thread.new",
            json!({"project": "api", "loop": {"kind": "implement", "task": "add it"}})
        ))
    );
    // A task needs `--implement`, and `--implement` a task: the parser says so.
    assert!(type_line(&mut st, "/new api --implement").is_empty());
    assert!(st
        .view
        .status
        .as_deref()
        .unwrap()
        .contains("takes the task"));
}

/// `/loop` starts no loop: a word that is no action points to `/implement` and `/review`.
#[test]
fn loop_starts_none_and_says_which_command_does() {
    let mut st = three_threads();
    st.view.width = 160;
    for line in ["/loop implement", "/loop gamma review", "/loop start"] {
        assert!(type_line(&mut st, line).is_empty(), "{line}");
        let said = st.view.status.as_deref().unwrap();
        assert!(
            said.contains(twr_core::slash::LOOP_STARTS_NONE),
            "{line}: {said}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The popup's `implementation loop…`
// ---------------------------------------------------------------------------------------------

/// `implementation loop…` puts `/implement <thread> ` in the box (in a dive into that thread,
/// `/implement `), ready for the task; a line already there goes to the history.
#[test]
fn the_popups_implementation_loop_fills_the_box() {
    let mut st = three_threads();
    st.view.width = 160;
    st.set_focus(Some("t-gamma".into()));
    read(&mut st);
    popup_open(&mut st);
    let rows = kinds(&st);
    let start = rows
        .iter()
        .position(|k| *k == ActionKind::LoopStart)
        .unwrap();
    assert_eq!(rows[start + 1], ActionKind::LoopImplement);
    let screen = text(&draw(&mut st, 160, 40));
    assert!(screen.contains(" implementation loop…"), "{screen}");
    st.handle_key(key(KeyCode::Esc));
    assert!(run_row(&mut st, ActionKind::LoopImplement).is_empty());
    assert_eq!(st.view.input.as_str(), "/implement gamma ");
    assert!(st.view.action.is_none());
    // Typed on, Enter starts it.
    for c in "add it".chars() {
        st.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        loop_call(&st.handle_key(key(KeyCode::Enter))),
        [(
            "loop.request",
            json!({"thread": "t-gamma", "kind": "implement", "task": "add it"})
        )]
    );
    // In a dive into the thread: no name; a line in the box goes to the history.
    st.dive("t-gamma");
    st.view.input.set("half a thought");
    // (A click opens the popup with a line in the box.)
    st.prefill_implement("t-gamma");
    assert_eq!(st.view.input.as_str(), "/implement ");
    assert_eq!(
        st.view.history.last().map(String::as_str),
        Some("half a thought")
    );
    assert!(st.view.status.as_deref().unwrap().contains("history"));
    // In a dive into another thread (whose words would all be the task), the dive is left
    // and the card selected, so the line names the thread.
    st.view.input.clear();
    st.dive("t-alpha");
    st.prefill_implement("t-gamma");
    assert_eq!(st.view.dive, None);
    assert_eq!(st.focused(), Some("t-gamma"));
    assert_eq!(st.view.input.as_str(), "/implement gamma ");
    assert!(st
        .view
        .status
        .as_deref()
        .unwrap()
        .contains("on gamma (out of the dive)"));
}

// ---------------------------------------------------------------------------------------------
// The setup card with a task
// ---------------------------------------------------------------------------------------------

/// The setup card of an implementation loop: ` IMPLEMENT LOOP `, its three options on the card
/// and in the popup (`run without a loop` between start and cancel), the task wrapped and
/// whole, `change…` after the options; `2` runs it without a loop, `3` cancels.
#[test]
fn the_setup_card_holds_the_task_and_has_three_options() {
    let mut st = three_threads();
    raise_impl_setup(&mut st, IMPL_SUMMARY);
    st.set_focus(Some("t-gamma".into()));
    let screen = read(&mut st);
    assert!(
        screen.contains("IMPLEMENT LOOP  first run: choose its"),
        "{screen}"
    );
    for part in [
        "1 start and save these settings",
        "2 run without a loop",
        "3 cancel",
    ] {
        assert!(screen.contains(part), "{part:?}: {screen}");
    }
    st.handle_key(key(KeyCode::Enter));
    let screen = text(&draw(&mut st, 160, 40));
    for part in [
        "IMPLEMENT LOOP  gamma: implementation loop setup (first run: choose its models).",
        "Task         add a CSV export to the reports page",
        "and a test that reads it back",
        "Plan check   contracts",
        "Commit       end",
        " 1 start and save these settings",
        " 2 run without a loop",
        " 3 cancel",
        " change…",
    ] {
        assert!(screen.contains(part), "{part:?}: {screen}");
    }
    assert_eq!(
        kinds(&st),
        [
            ActionKind::Answer(1),
            ActionKind::Answer(2),
            ActionKind::Answer(3),
            ActionKind::LoopChange,
            ActionKind::Talk,
            ActionKind::Pin,
            ActionKind::Stop,
            ActionKind::Park,
        ]
    );
    // Read: `2` is `run without a loop`, at the revision shown.
    let rev = st.top_attention("t-gamma").unwrap().revision;
    st.note_drawn(Instant::now());
    if let Some(s) = st.view.shown.as_mut() {
        s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
    }
    assert_eq!(
        st.handle_key(key(KeyCode::Char('2'))),
        [Action::Respond {
            attn_id: "ls2".into(),
            revision: rev,
            decision: "run without a loop".into(),
        }]
    );
}

/// The task's row wraps under its value and is never cut, at any width; every row within it.
#[test]
fn the_task_wraps_under_its_value_and_is_never_cut() {
    let mut st = three_threads();
    raise_impl_setup(&mut st, IMPL_SUMMARY);
    let item = st.top_attention("t-gamma").unwrap().clone();
    for w in [20usize, 30, 46, 80] {
        let rows = flat(&crate::loops::setup_rows(&item, w));
        for r in &rows {
            assert!(crate::edit::width(r) <= w, "{w}: {r}");
            assert!(!r.contains('…'), "{w}: cut: {r}");
        }
        let all = rows.join(" ");
        let words = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        let task = "add a CSV export to the reports page, with a header row, one row per report \
                    and the totals last, and a test that reads it back";
        let said = words(&all).join(" ");
        assert!(said.contains(task), "{w}: {said}");
    }
    let rows = flat(&crate::loops::setup_rows(&item, 46));
    let at = rows.iter().position(|r| r.starts_with("Task")).unwrap();
    assert_eq!(rows[at], "Task         add a CSV export to the reports");
    assert!(rows[at + 1].starts_with("             page,"), "{rows:?}");
    // A task of several lines, as the hub indents its next ones: under the value.
    let mut many = item.clone();
    many.summary = "gamma: implementation loop setup (check its settings). Start?\nTask         \
                    first line\n             second line\nProfile      low"
        .into();
    let rows = flat(&crate::loops::setup_rows(&many, 46));
    let at = rows.iter().position(|r| r.starts_with("Task")).unwrap();
    assert_eq!(rows[at], "Task         first line");
    assert_eq!(rows[at + 1], "             second line");
    assert_eq!(rows[at + 2], "Profile      low");
}

/// An implementer whose runtime runs no subagents (`LoopSetup::no_workers`): the hub's card
/// has no Workers row, and the card draws none, the Reviewer row right after the
/// Implementer's.
#[test]
fn a_card_without_workers_draws_no_workers_row() {
    let mut st = three_threads();
    let summary: String = IMPL_SUMMARY
        .lines()
        .filter(|l| !l.starts_with("Workers"))
        .collect::<Vec<_>>()
        .join("\n");
    raise_impl_setup(&mut st, &summary);
    let item = st.top_attention("t-gamma").unwrap().clone();
    let rows = flat(&crate::loops::setup_rows(&item, 80));
    assert!(!rows.iter().any(|r| r.contains("Workers")), "{rows:#?}");
    let at = rows
        .iter()
        .position(|r| r.starts_with("Implementer"))
        .unwrap();
    assert!(rows[at + 1].starts_with("Reviewer     codex"), "{rows:#?}");
    // The card with the row, as before.
    let mut with = item.clone();
    with.summary = IMPL_SUMMARY.into();
    let rows = flat(&crate::loops::setup_rows(&with, 80));
    assert!(
        rows.contains(&"Workers      claude-opus-5-5 · high".to_string()),
        "{rows:#?}"
    );
}

/// A held task's card in the compact form (a card's one row of options) still has all three,
/// wrapped within the card.
#[test]
fn the_cards_compact_form_keeps_three_options() {
    let mut st = three_threads();
    raise_impl_setup(&mut st, IMPL_SUMMARY);
    let t = st.registry.get("t-gamma").unwrap().clone();
    for width in [30usize, 46, 80] {
        let body = card_body(&st, &t, width, None);
        let rows = flat(&card_rows_counted(body, 12, width as u16).0);
        let all = rows.join("\n");
        for part in ["1 start and save", "2 run without a loop", "3 cancel"] {
            assert!(
                all.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .contains(part),
                "{width}: {part:?}: {rows:#?}"
            );
        }
        for r in &rows {
            assert!(crate::edit::width(r) <= width, "{width}: {r}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------------------------

/// The editor on an implementation loop's card: its title, the plan check and commit rows
/// (no review commit), each cycling; Save sends them with the draft.
#[test]
fn the_editor_of_an_implementation_card_has_plan_check_and_commit() {
    let mut st = three_threads();
    raise_impl_setup(&mut st, IMPL_SUMMARY);
    st.set_focus(Some("t-gamma".into()));
    run_row(&mut st, ActionKind::LoopChange);
    let card = json!({"attn_id": "ls2", "revision": 4, "thread": "t-gamma",
        "setup": {"kind": "implement", "first_run": true, "why": "first_run",
                  "settings": {"profile": "medium",
                               "reviewer": {"runtime": "codex", "model": "gpt-6.1-sol"},
                               "workers": {"model": "claude-opus-5-5", "effort": "high"}},
                  "save_for": "all", "project": "proj", "task": "add a CSV export",
                  "profile": twr_core::loops::config::Profile::builtin("medium").unwrap(),
                  "warnings": [], "problems": []},
        "summary": IMPL_SUMMARY,
        "options": ["start and save these settings", "run without a loop", "cancel"]});
    st.apply_result(
        "loop.setup.get",
        &json!({"asked": {"attn_id": "ls2"}, "result": card}),
    );
    st.apply_result(
        "loop.settings.get",
        &json!({"asked": {}, "result": settings_got()}),
    );
    let e = st.view.loop_editor.as_ref().unwrap();
    assert_eq!(e.kind, Some(LoopKind::Implement));
    let rows = crate::loops::rows(e, Some("fake"));
    let keys: Vec<RowKey> = rows.iter().map(|r| r.key).collect();
    assert!(keys.contains(&RowKey::PlanCheck), "{keys:?}");
    assert!(keys.contains(&RowKey::ImplCommit), "{keys:?}");
    assert!(!keys.contains(&RowKey::ReviewCommit), "{keys:?}");
    let value = |st: &AppState, k: RowKey| {
        let e = st.view.loop_editor.as_ref().unwrap();
        crate::loops::rows(e, Some("fake"))
            .into_iter()
            .find(|r| r.key == k)
            .unwrap()
            .value
    };
    assert_eq!(value(&st, RowKey::PlanCheck), "the profile's (contracts)");
    assert_eq!(value(&st, RowKey::ImplCommit), "end");
    let screen = text(&draw(&mut st, 120, 50));
    assert!(screen.contains(" implementation loop setup "), "{screen}");
    change(&mut st, RowKey::PlanCheck, KeyCode::Right);
    assert_eq!(value(&st, RowKey::PlanCheck), "contracts");
    change(&mut st, RowKey::PlanCheck, KeyCode::Right);
    assert_eq!(value(&st, RowKey::PlanCheck), "always");
    change(&mut st, RowKey::ImplCommit, KeyCode::Right);
    assert_eq!(value(&st, RowKey::ImplCommit), "per-round");
    change(&mut st, RowKey::ImplCommit, KeyCode::Left);
    change(&mut st, RowKey::ImplCommit, KeyCode::Left);
    assert_eq!(value(&st, RowKey::ImplCommit), "none");
    let saved = loop_call(&st.handle_key(key(KeyCode::Char('s'))));
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].0, "loop.setup.update");
    assert_eq!(saved[0].1["revision"], 4);
    assert_eq!(saved[0].1["settings"]["plan_check"], "always");
    assert_eq!(saved[0].1["settings"]["commit"], "none");
}

/// A row's dim line that wraps continues under the value column, as its first line starts.
#[test]
fn an_editor_rows_wrapped_hint_stays_under_the_value() {
    let mut st = three_threads();
    raise_impl_setup(&mut st, IMPL_SUMMARY);
    st.set_focus(Some("t-gamma".into()));
    run_row(&mut st, ActionKind::LoopChange);
    let card = json!({"attn_id": "ls2", "revision": 1, "thread": "t-gamma",
        "setup": {"kind": "implement", "settings": {"profile": "medium",
                  "reviewer": {"runtime": "codex", "model": "gpt-6.1-sol"},
                  "workers": {"model": "claude-opus-5-5"}}}});
    st.apply_result(
        "loop.setup.get",
        &json!({"asked": {"attn_id": "ls2"}, "result": card}),
    );
    st.apply_result(
        "loop.settings.get",
        &json!({"asked": {}, "result": settings_got()}),
    );
    let screen = lines(&draw(&mut st, 90, 60));
    let at = screen
        .iter()
        .position(|l| l.contains("the card shows these; 1 on it"))
        .unwrap_or_else(|| panic!("{}", screen.join("\n")));
    let first = screen[at].find("the card").unwrap();
    let next = &screen[at + 1];
    let starts = next
        .char_indices()
        .skip(1)
        .find(|(_, c)| !c.is_whitespace() && *c != '│')
        .map(|(i, _)| i)
        .unwrap();
    assert_eq!(starts, first, "{}\n{}", screen[at], next);
    // A row that fits is not cut: no `…` at its end.
    assert!(
        !screen.iter().any(|l| l.contains("…│")),
        "{}",
        screen.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// The loop line and the popup per phase
// ---------------------------------------------------------------------------------------------

/// The loop line of each of an implementation loop's first phases, at several widths: in full
/// (`IMPLEMENT LOOP  medium · plan`), without the profile, short (`plan`, `p1`, `impl`), the
/// one-row card's form; every row within the width.
#[test]
fn the_loop_line_of_each_phase_at_several_widths() {
    for (upto, full, bare, short) in [
        (Upto::Plan, "medium · plan", "plan", "plan"),
        (Upto::PlanCheck, "medium · plan check", "plan check", "p1"),
        (
            Upto::Implement,
            "medium · implementing · ledger 1",
            "implementing · ledger 1",
            "impl · ldg 1",
        ),
        (
            Upto::Ladder,
            "medium · ladder m1 · ledger 1",
            "ladder m1 · ledger 1",
            "m1 · ldg 1",
        ),
    ] {
        let mut st = three_threads();
        implementing(&mut st, "t-gamma", upto);
        let l = st.registry.get("t-gamma").unwrap().r#loop.clone().unwrap();
        assert_eq!(l.kind, LoopKind::Implement);
        let at = |w: usize| flat(&[crate::loops::card_line(&l, w)])[0].clone();
        assert_eq!(at(80), format!(" IMPLEMENT LOOP  {full}"), "{upto:?}");
        let room = 17 + crate::edit::width(full);
        if bare != full {
            assert_eq!(at(room - 1), format!(" IMPLEMENT LOOP  {bare}"), "{upto:?}");
        }
        assert_eq!(
            at(17 + crate::edit::width(short)),
            format!(" IMPLEMENT LOOP  {short}"),
            "{upto:?}"
        );
        for w in [6usize, 12, 20, 30, 46] {
            assert!(crate::edit::width(&at(w)) <= w, "{upto:?} {w}");
        }
        let one = flat(&[crate::loops::short_line(&l, "running", 60)])[0].clone();
        let word = match upto {
            Upto::Ladder => "m1",
            Upto::Implement => "impl",
            _ => short,
        };
        assert_eq!(
            one,
            format!(" IMPLEMENT LOOP  {word} · running…"),
            "{upto:?}"
        );
        // On the card, first.
        let t = st.registry.get("t-gamma").unwrap().clone();
        let body = card_body(&st, &t, 46, None);
        let rows = flat(&card_rows_counted(body, 4, 46).0);
        assert!(
            rows[0].starts_with(" IMPLEMENT LOOP "),
            "{upto:?}: {rows:?}"
        );
    }
}

/// The popup's progress per phase: the phase strip with the current one marked, the plan
/// check skipped when it did not run; pause, stop in every phase, `skip to verify` only once
/// there is a ladder to end (`/loop skip` says why before it).
#[test]
fn the_popup_rows_and_progress_per_phase() {
    for (upto, strip) in [
        (
            Upto::Plan,
            "▸plan · plan check · implement · ladder · sweep · verify · report",
        ),
        (
            Upto::PlanCheck,
            "plan · ▸plan check · implement · ladder · sweep · verify · report",
        ),
        (
            Upto::Implement,
            "plan · plan check · ▸implement · ladder · sweep · verify · report",
        ),
        (
            Upto::Ladder,
            "plan · plan check · implement · ▸ladder · sweep · verify · report",
        ),
    ] {
        let mut st = three_threads();
        st.view.width = 160;
        implementing(&mut st, "t-gamma", upto);
        st.set_focus(Some("t-gamma".into()));
        popup_open(&mut st);
        let rows = kinds(&st);
        assert!(rows.contains(&ActionKind::LoopPause), "{upto:?}");
        assert!(rows.contains(&ActionKind::LoopStop), "{upto:?}");
        assert_eq!(
            rows.contains(&ActionKind::LoopSkip),
            upto == Upto::Ladder,
            "{upto:?}: {rows:?}"
        );
        let screen = text(&draw(&mut st, 160, 40));
        assert!(screen.contains(strip), "{upto:?}: {screen}");
        if upto >= Upto::Implement {
            assert!(screen.contains("rounds p1 1(0)"), "{upto:?}: {screen}");
        }
        st.handle_key(key(KeyCode::Esc));
        let skip = loop_call(&type_line(&mut st, "/loop gamma skip"));
        if upto == Upto::Ladder {
            assert_eq!(
                skip,
                [(
                    "loop.control",
                    json!({"thread": "t-gamma", "action": "skip_to_verify"})
                )]
            );
        } else {
            assert!(skip.is_empty(), "{upto:?}");
            assert_eq!(
                st.view.status.as_deref(),
                Some(
                    "gamma: the loop has not implemented its plan yet: skip to verify comes \
                     after it"
                ),
                "{upto:?}"
            );
        }
        // Stop asks `y`, naming the kind.
        assert!(loop_call(&type_line(&mut st, "/loop gamma stop")).is_empty());
        assert!(st
            .view
            .confirm
            .as_ref()
            .unwrap()
            .prompt
            .contains("stop gamma's implementation loop?"));
        st.handle_key(key(KeyCode::Char('n')));
    }
    // A plan with no contract risk under `contracts`: the plan check is skipped.
    let mut st = three_threads();
    let mut started = impl_started();
    started.settings.plan_check = None;
    let mut run = ImplRun::new("t-gamma", 500, started);
    run.turn(
        "plan",
        "## Goal\nCSV.\n\n```twr-plan\n{\"contract_risk\": false, \"reason\": \"none\"}\n```\n",
    );
    control(&mut st, &run.evs);
    let t = st.registry.get("t-gamma").unwrap();
    assert!(
        crate::loops::status_text(t).contains("plan · plan check skipped · ▸implement · ladder"),
        "{}",
        crate::loops::status_text(t)
    );
}

/// While the plan check's review run goes, the thread is in the background, its activity the
/// reviewer and how long it has run.
#[test]
fn the_plan_checks_run_reads_as_the_reviewer() {
    let mut st = three_threads();
    let mut run = implementing(&mut st, "t-gamma", Upto::PlanCheck);
    let mut task = hub(
        &mut run.s,
        EventBody::TaskUpdated(TaskUpdated {
            task_id: "loop:l2".into(),
            running: true,
            description: "reviewer gpt-6.1-sol medium".into(),
            ..Default::default()
        }),
    );
    task.ts = chrono::Utc::now() - chrono::Duration::seconds(2 * 60 + 5);
    control(&mut st, &[task]);
    let screen = read(&mut st);
    assert!(
        screen.contains(" IMPLEMENT LOOP  medium · plan check"),
        "{screen}"
    );
    let at = screen
        .lines()
        .position(|l| l.contains("background · reviewer gpt-6.1-sol medium"))
        .unwrap_or_else(|| panic!("{screen}"));
    let both = format!(
        "{}{}",
        screen.lines().nth(at).unwrap(),
        screen.lines().nth(at + 1).unwrap()
    );
    assert!(both.contains("2m"), "{screen}");
    st.set_focus(Some("t-gamma".into()));
    popup_open(&mut st);
    let screen = text(&draw(&mut st, 160, 40));
    assert!(screen.contains("rounds p1 ▸"), "{screen}");
    assert!(
        screen.contains("waiting on the reviewer: gpt-6.1-sol medium"),
        "{screen}"
    );
}

/// The stuck card on an implementation loop's step: its badge and options, and the popup's
/// progress says why it is stuck.
#[test]
fn a_stuck_implement_step() {
    let mut run = ImplRun::new("t-gamma", 500, impl_started());
    run.to(Upto::Implement, true);
    for _ in 0..2 {
        run.turn(
            "implement",
            "Half.\n```twr-implement\n{\"status\": \"partial\", \"gates\": {\"ran\": [\"cargo \
             test\"], \"result\": \"fail\"}}\n```\n",
        );
    }
    let ext = json!({ "twr": { LOOP_STUCK_KEY: { "loop": "l2",
        "why": "the implement turn's gates are red twice" } } });
    let ev = hub(
        &mut run.s,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "st2".into(),
            reason: AttentionReason::Error,
            summary: "gamma's implementation loop is stuck: the gates are red".into(),
            options: vec!["resume".into(), "stop".into()],
            ext: ext.as_object().cloned(),
            ..Default::default()
        }),
    );
    let mut st = three_threads();
    control(&mut st, &run.evs);
    control(&mut st, &[ev]);
    let l = st.registry.get("t-gamma").unwrap().r#loop.clone().unwrap();
    assert!(l.stuck.is_some(), "{l:?}");
    assert_eq!(
        flat(&[crate::loops::card_line(&l, 80)])[0],
        " IMPLEMENT LOOP  medium · implementing · ledger 1 · stuck"
    );
    let screen = read(&mut st);
    assert!(
        screen.contains(" IMPLEMENT LOOP  impl · ldg 1 · stuck"),
        "{screen}"
    );
    assert!(screen.contains("LOOP STUCK"), "{screen}");
    st.set_focus(Some("t-gamma".into()));
    popup_open(&mut st);
    assert_eq!(
        kinds(&st)[..2],
        [ActionKind::Answer(1), ActionKind::Answer(2)]
    );
}

// ---------------------------------------------------------------------------------------------
// The dive's folded rows, and how a loop ended
// ---------------------------------------------------------------------------------------------

/// A whole implementation loop in a dive: each loop prompt folds to a row naming the kind and
/// the step; the plan, the plan check's round, the implement turn and the commit to one row
/// each, which Ctrl-O opens.
#[test]
fn a_dive_folds_a_whole_implementation_loop() {
    let mut run = ImplRun::new("t-api", 500, impl_started());
    run.to(Upto::Ended, true);
    let mut tr = Transcript::new("t-api");
    tr.settle();
    for ev in &run.evs {
        tr.apply(ev);
    }
    let expanded = HashSet::new();
    let view = |all: bool| -> Vec<String> {
        let collapse = Collapse {
            on: true,
            quiet_ends: false,
            all,
            expanded: Some(&expanded),
        };
        transcript_view(&tr.entries, 120, &collapse, &Threads::default())
            .into_iter()
            .map(|(l, ..)| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    };
    let folded: Vec<String> = view(false)
        .into_iter()
        .map(|r| r.trim().to_string())
        .filter(|r| r.contains("loop") || r.starts_with('↻'))
        .collect();
    assert_eq!(
        folded,
        [
            "implementation loop · plan (ctrl+o to expand)",
            "↻ plan · revision 1 · contract risk: no (ctrl+o to expand)",
            "↻ plan check p1 · gpt-6.1-sol medium · 1s · 1 finding · 1 dismissed (ctrl+o to \
             expand)",
            "implementation loop · triage p1 (ctrl+o to expand)",
            "implementation loop · implement (ctrl+o to expand)",
            "↻ implement · gates pass (ctrl+o to expand)",
            "↻ review round m1 · gpt-6.1-sol medium · 1s · no findings",
            "↻ review round v1 · gpt-6.1-sol xhigh · 1s · no findings",
            "implementation loop · report (ctrl+o to expand)",
            "↻ committed abc1234 (ctrl+o to expand)",
        ],
        "{folded:#?}"
    );
    let open = view(true);
    for part in [
        "⎿ plan.md: /data/loops/l2/plan.md",
        "⎿ contract risk: a new endpoint, no schema change",
        "• p1.1 major src/a.rs:4 — problem 1",
        "⎿ dismissed: the plan covers it",
        "⎿ ran cargo test --workspace",
        "⎿ /w/api abc1234",
    ] {
        assert!(open.iter().any(|r| r.contains(part)), "{part:?}: {open:#?}");
    }
}

/// A commit whose check failed twice: `not committed:` and why, in red; the loop's end says
/// so on the card and in the status bar.
#[test]
fn a_loop_that_did_not_commit_says_so() {
    let mut run = ImplRun::new("t-gamma", 500, impl_started());
    run.to(Upto::Ended, false);
    let mut tr = Transcript::new("t-gamma");
    tr.settle();
    for ev in &run.evs {
        tr.apply(ev);
    }
    let rows: Vec<String> = tr
        .entries
        .iter()
        .filter_map(|e| match e {
            crate::app::Entry::LoopStep { note, .. } => Some(crate::loops::step_head(note)),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows[rows.len() - 2..],
        [
            "not committed: /w/api: 1 path is uncommitted beyond the baseline: a.rs",
            "not committed: /w/api: 1 path is uncommitted beyond the baseline: a.rs",
        ]
    );
    // A dive draws the commit row in red.
    let mut st = three_threads();
    control(&mut st, &run.evs);
    let t = st.registry.get("t-gamma").unwrap();
    assert_eq!(
        crate::loops::status_text(t),
        "gamma runs no loop; its last implementation loop ended clean; not committed: /w/api: 1 \
         path is uncommitted beyond the baseline: a.rs"
    );
    let screen = read(&mut st);
    assert!(
        screen.contains(" IMPLEMENT LOOP  clean · not committed"),
        "{screen}"
    );
}

/// How a loop ended, on the card until the thread's next turn and in `/loop`'s status:
/// `ended clean · committed abc1234`, shorter on a narrow card.
#[test]
fn how_a_loop_ended_on_the_card_and_in_the_status_bar() {
    let mut st = three_threads();
    st.view.width = 160;
    let mut run = implementing(&mut st, "t-gamma", Upto::Ended);
    let t = st.registry.get("t-gamma").unwrap().clone();
    assert!(t.r#loop.is_none());
    let last = crate::loops::ended_due(&t).expect("the card says how it ended");
    let at = |w: usize| flat(&[crate::loops::ended_line(last, w)])[0].clone();
    assert_eq!(at(80), " IMPLEMENT LOOP  ended clean · committed abc1234");
    assert_eq!(at(42), " IMPLEMENT LOOP  clean · committed abc1234");
    assert_eq!(at(32), " IMPLEMENT LOOP  clean · abc1234");
    for w in [8usize, 16, 24] {
        assert!(crate::edit::width(&at(w)) <= w, "{w}");
    }
    let screen = read(&mut st);
    assert!(
        screen.contains(" IMPLEMENT LOOP  clean · committed abc1234"),
        "{screen}"
    );
    type_line(&mut st, "/loop gamma");
    assert_eq!(
        st.view.status.as_deref(),
        Some("gamma runs no loop; its last implementation loop ended clean; committed abc1234")
    );
    // The thread's next turn: the card no longer says it.
    control(&mut st, &[run.s.turn_started("next")]);
    let t = st.registry.get("t-gamma").unwrap();
    assert!(crate::loops::ended_due(t).is_none());
    let screen = read(&mut st);
    assert!(!screen.contains("IMPLEMENT LOOP"), "{screen}");
}

/// A loop that committed some repositories and not another: the commits first, then what was
/// not committed; never `not committed` alone, on the card, in the status bar or in the
/// transcript's commit row, which also lists the check's notes, after its problems.
#[test]
fn a_loop_that_committed_some_repositories_says_both() {
    use twr_core::loops::contract::Commit;
    let commit = |repo: &str, sha: &str| Commit {
        repo: repo.into(),
        sha: sha.into(),
    };
    let last = twr_core::reduce::LastLoop {
        id: "l2".into(),
        kind: LoopKind::Implement,
        outcome: twr_core::loops::Outcome::Clean,
        ended_turn: Some(4),
        commits: vec![commit("/w/api", "abc1234def")],
        commit_problem: Some("/w/web: 1 path is uncommitted beyond the baseline: a.rs".into()),
        requested_by: Default::default(),
    };
    assert_eq!(
        crate::loops::commit_words(&last, true).as_deref(),
        Some("committed abc1234 · not committed: /w/web: 1 path is uncommitted beyond the baseline: a.rs")
    );
    assert_eq!(
        crate::loops::commit_words(&last, false).as_deref(),
        Some("committed abc1234 · not committed in web")
    );
    let at = |w: usize| flat(&[crate::loops::ended_line(&last, w)])[0].clone();
    assert_eq!(
        at(80),
        " IMPLEMENT LOOP  ended clean · committed abc1234 · not committed in web"
    );
    assert_eq!(
        at(60),
        " IMPLEMENT LOOP  clean · abc1234 · not all committed"
    );
    for w in [8usize, 16, 24, 32] {
        let line = at(w);
        assert!(crate::edit::width(&line) <= w, "{w}");
        assert!(!line.contains("· not committed"), "{line}");
    }
    // Several repositories committed, two not: each named.
    let more = twr_core::reduce::LastLoop {
        commits: vec![
            commit("/w/api", "abc1234def"),
            commit("/w/lib", "0123456789"),
        ],
        commit_problem: Some("/w/web: dirty; /w/docs: 2 paths are uncommitted".into()),
        ..last.clone()
    };
    assert_eq!(
        crate::loops::commit_words(&more, false).as_deref(),
        Some("committed abc1234 in api, 0123456 in lib · not committed in web, docs")
    );
    // Only problems: `not committed`, as before.
    let none = twr_core::reduce::LastLoop {
        commits: Vec::new(),
        ..last.clone()
    };
    assert_eq!(
        crate::loops::commit_words(&none, false).as_deref(),
        Some("not committed")
    );
    // The transcript's commit row: both, red for the problem; opened, the commit, the
    // problem, then the notes.
    let note = crate::loops::StepNote::Commit(twr_core::event::LoopCommitChecked {
        id: "l2".into(),
        commits: vec![commit("/w/api", "abc1234def")],
        problems: vec!["/w/web: 1 path is uncommitted beyond the baseline: a.rs".into()],
        notes: vec![
            "the thread wrote no twr-commit block (the check reads git, not the block)".into(),
        ],
        ..Default::default()
    });
    assert_eq!(
        crate::loops::step_head(&note),
        "committed abc1234 · not committed: /w/web: 1 path is uncommitted beyond the baseline: a.rs"
    );
    assert!(crate::loops::step_failed(&note));
    assert_eq!(
        crate::loops::step_rows(&note),
        [
            "/w/api abc1234",
            "/w/web: 1 path is uncommitted beyond the baseline: a.rs",
            "note: the thread wrote no twr-commit block (the check reads git, not the block)",
        ]
    );
    // A check that passed with a note: the note, no problem.
    let passed = crate::loops::StepNote::Commit(twr_core::event::LoopCommitChecked {
        id: "l2".into(),
        commits: vec![commit("/w/api", "abc1234def")],
        notes: vec!["the thread wrote no twr-commit block".into()],
        ..Default::default()
    });
    assert_eq!(crate::loops::step_head(&passed), "committed abc1234");
    assert!(!crate::loops::step_failed(&passed));
    assert_eq!(
        crate::loops::step_rows(&passed),
        [
            "/w/api abc1234",
            "note: the thread wrote no twr-commit block"
        ]
    );
}
