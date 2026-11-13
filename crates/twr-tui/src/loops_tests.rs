//! Review loops in the TUI ([`crate::loops`]): the loop line on a card, the setup card and the
//! stuck card, the action popup's loop rows, `/review` and `/loop`, the settings editor and its
//! requests, and the Settings overlay's Loops row. Driven as the IO loop drives the TUI: a
//! frame before each key that answers what it showed.

use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use serde_json::{json, Value};
use twr_core::event::{
    AttentionRaised, AttentionReason, Event, EventBody, LoopStarted, Source, TaskUpdated,
    LOOP_SETUP_KEY, LOOP_STUCK_KEY,
};
use twr_core::loops::config::{Profile, ReviewerChoice};
use twr_core::loops::contract::{Finding, Severity};
use twr_core::loops::settings::{presets, LoopSettings};
use twr_core::loops::{Action as Step, Input, LoopKind, LoopSetup, LoopState};
use twr_core::testkit::ThreadScript;

use crate::app::tests::*;
use crate::app::{Action, ActionKind, AppState, CARD_SETTLE};
use crate::loops::{EditorFor, RowKey};

/// `body` as the hub records it on `s`'s thread.
pub(crate) fn hub(s: &mut ThreadScript, body: EventBody) -> Event {
    let mut ev = s.event(body);
    ev.source = Source::Hub;
    ev
}

/// What `loop.started` says of a `high` review loop reviewed by codex.
pub(crate) fn started() -> LoopStarted {
    LoopStarted {
        id: "l1".into(),
        kind: LoopKind::Review,
        profile: Profile::builtin("high").unwrap(),
        max_rounds_per_tier: 25,
        settings: LoopSettings {
            profile: "high".into(),
            reviewer: ReviewerChoice::parse("codex/gpt-6.1-sol").unwrap(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// `n` findings, as a reviewer answers them.
fn findings(n: usize) -> Vec<Finding> {
    (0..n)
        .map(|i| Finding {
            severity: Severity::Major,
            location: format!("src/a.rs:{}", i + 4),
            problem: format!("problem {}\nmore", i + 1),
            fix: "fix it".into(),
            ..Default::default()
        })
        .collect()
}

/// Starts a loop on `thread` and runs it, as the hub's driver records it, to round m2's review
/// run: m1 answered 2 findings, one fixed as a new class, one dismissed.
pub(crate) fn looping(st: &mut AppState, thread: &str) -> LoopState {
    let mut s = ThreadScript::new(thread);
    s.at(500);
    let now = twr_core::testkit::base_time();
    let mut evs = Vec::new();
    let (mut m, first) = LoopState::start(&started()).unwrap();
    evs.push(hub(&mut s, EventBody::LoopStarted(Box::new(started()))));
    let feed = |s: &mut ThreadScript, evs: &mut Vec<Event>, actions: Vec<Step>| {
        for a in actions {
            if let Step::Record(body) = a {
                evs.push(hub(s, body));
            }
        }
    };
    feed(&mut s, &mut evs, first);
    let step = |m: &mut LoopState, s: &mut ThreadScript, evs: &mut Vec<Event>, input| {
        let actions = m.step(input, now).unwrap();
        feed(s, evs, actions);
    };
    step(
        &mut m,
        &mut s,
        &mut evs,
        Input::GoalWritten { goal: "G".into() },
    );
    for (round, n) in [("m1", 2), ("m2", 0)] {
        step(
            &mut m,
            &mut s,
            &mut evs,
            Input::ReviewStarted {
                round: round.into(),
                diffs: vec![],
                prompt_digest: None,
            },
        );
        if n == 0 {
            break;
        }
        step(
            &mut m,
            &mut s,
            &mut evs,
            Input::ReviewAnswered {
                round: round.into(),
                findings: findings(n),
                answer_path: None,
                usage: None,
                unchanged: false,
            },
        );
        let reply = "done\n\n```twr-triage\n{\"items\": [{\"id\": \"m1.1\", \"verdict\": \
                     \"fixed\", \"class\": \"new-class\", \"rule\": \"r\"}, {\"id\": \"m1.2\", \
                     \"verdict\": \"dismissed\", \"reason\": \"fine\"}], \"gates\": {\"ran\": \
                     [\"cargo test\"], \"result\": \"pass\"}}\n```\n";
        let input = m.reply_input(reply).unwrap();
        step(&mut m, &mut s, &mut evs, input);
    }
    control(st, &evs);
    m
}

/// A setup card's summary, as the hub writes it (`hub::loops::card_text`).
pub(crate) const SETUP_SUMMARY: &str = "gamma: review loop setup (first run: choose its models). \
Start and save these settings?
Profile      high · medium tier, then xhigh tier · sweep · 5 verify
Scope        branch · 7 commits, 23 files
Implementer  fake · its default model (the thread's own)
Workers      claude-opus-5-5 · high
Reviewer     codex · gpt-6.1-sol · medium → xhigh
Save for     all projects
Warning: reviewer and implementer are the same family: it tends to miss what the implementer \
missed";

/// Raises a setup card (`ls1`) on `t-gamma`, as the hub raises one.
pub(crate) fn raise_setup(st: &mut AppState) {
    let setup = LoopSetup {
        first_run: true,
        project: Some("proj".into()),
        ..Default::default()
    };
    let mut g = ThreadScript::new("t-gamma");
    g.at(600);
    let ext = json!({ "twr": { LOOP_SETUP_KEY: setup } });
    let ev = hub(
        &mut g,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "ls1".into(),
            reason: AttentionReason::Review,
            summary: SETUP_SUMMARY.into(),
            options: vec!["start and save these settings".into(), "cancel".into()],
            ext: ext.as_object().cloned(),
            ..Default::default()
        }),
    );
    control(st, &[ev]);
}

/// Raises a stuck item (`st1`) on `t-gamma`.
pub(crate) fn raise_stuck(st: &mut AppState) {
    let mut g = ThreadScript::new("t-gamma");
    g.at(600);
    let ext = json!({ "twr": { LOOP_STUCK_KEY: { "loop": "l1", "why": "the review run failed twice" } } });
    let ev = hub(
        &mut g,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "st1".into(),
            reason: AttentionReason::Error,
            summary: "gamma's review loop is stuck: the review run failed twice".into(),
            options: vec!["resume".into(), "stop".into()],
            ext: ext.as_object().cloned(),
            ..Default::default()
        }),
    );
    control(st, &[ev]);
}

fn lines(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (0..a.height)
        .map(|y| (0..a.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn text(buf: &Buffer) -> String {
    lines(buf).join("\n")
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

/// The rows of the action popup open on the selection.
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

/// What `loop.settings.get` answers: claude-code and codex installed and logged in, grok-build
/// not installed, nothing saved.
pub(crate) fn settings_got() -> Value {
    json!({
        "settings": null,
        "source": null,
        "first_card": LoopSettings {
            profile: "high".into(),
            reviewer: ReviewerChoice::parse("codex/gpt-6.1-sol").unwrap(),
            workers: twr_core::loops::config::WorkersChoice::parse("claude-opus-5-5:high").unwrap(),
            ..Default::default()
        },
        "projects": [],
        "presets": presets(),
        "profiles": ["low", "medium", "high", "deep"],
        "runtimes": [
            {"runtime": "claude-code", "found": true, "login": true,
             "models": ["claude-opus-5-5", "claude-sonnet-5-5"], "efforts": ["low", "medium", "high"],
             "subagents": true, "accounts": []},
            {"runtime": "codex", "found": true, "login": true,
             "models": ["gpt-6.1-sol", "gpt-6.1-mini"], "efforts": ["medium", "high", "xhigh"],
             "subagents": false, "accounts": ["work", "home"]},
            {"runtime": "grok-build", "found": false, "login": false,
             "models": [], "efforts": [], "subagents": false, "accounts": []},
        ],
        "warnings": [],
        "problems": [],
    })
}

/// The editor's rows as (label, value).
fn editor_rows(st: &AppState) -> Vec<(RowKey, String)> {
    let e = st.view.loop_editor.as_ref().unwrap();
    let rt = st.loop_editor_thread_runtime();
    crate::loops::rows(e, rt.as_deref())
        .into_iter()
        .map(|r| (r.key, r.value))
        .collect()
}

fn value(st: &AppState, key: RowKey) -> String {
    editor_rows(st)
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no {key:?} row"))
}

/// Selects the editor's row `key` (↓ from the first) and steps it with `code`.
pub(crate) fn change(st: &mut AppState, key: RowKey, code: KeyCode) -> Vec<Action> {
    st.handle_key(crate::app::tests::key(KeyCode::Home));
    while st.loop_editor_selected() != Some(key) {
        let before = st.loop_editor_selected();
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        assert_ne!(st.loop_editor_selected(), before, "no {key:?} row");
    }
    st.handle_key(crate::app::tests::key(code))
}

// ---------------------------------------------------------------------------------------------
// The card and the popup
// ---------------------------------------------------------------------------------------------

/// A thread in a loop: its card's section starts with the badge and the loop line, the state
/// under it; its popup has the loop's controls and shows its progress; `stop loop` asks `y`.
#[test]
fn a_looping_card_and_its_popup() {
    let mut st = three_threads();
    looping(&mut st, "t-gamma");
    let screen = read(&mut st);
    let at = screen
        .lines()
        .position(|l| l.contains("REVIEW LOOP  high · ladder m2 · ledger 1"))
        .unwrap_or_else(|| panic!("no loop line: {screen}"));
    let under = screen.lines().nth(at + 1).unwrap();
    assert!(under.contains("running"), "the state under it: {under}");
    st.set_focus(Some("t-gamma".into()));
    popup_open(&mut st);
    assert_eq!(
        kinds(&st),
        [
            ActionKind::Talk,
            ActionKind::LoopPause,
            ActionKind::LoopStop,
            ActionKind::LoopSkip,
            ActionKind::LoopSettings,
            ActionKind::Pin,
            ActionKind::Stop,
            ActionKind::Park,
            ActionKind::Attach,
        ]
    );
    let screen = text(&draw(&mut st, 160, 40));
    for part in [
        "scope · ▸ladder · sweep · verify · report",
        "rounds m1 2(1) · m2 ▸",
        "waiting on the reviewer: gpt-6.1-sol medium",
        " pause loop",
        " stop loop",
        " skip to verify",
        " loop settings…",
    ] {
        assert!(screen.contains(part), "{part:?}: {screen}");
    }
    st.handle_key(crate::app::tests::key(KeyCode::Esc));
    // Pause: sent at once. Stop: asks `y` first.
    assert_eq!(
        loop_call(&run_row(&mut st, ActionKind::LoopPause)),
        [(
            "loop.control",
            json!({"thread": "t-gamma", "action": "pause"})
        )]
    );
    assert!(run_row(&mut st, ActionKind::LoopStop).is_empty());
    assert!(st
        .view
        .confirm
        .as_ref()
        .unwrap()
        .prompt
        .contains("stop gamma's review loop?"));
    assert_eq!(
        loop_call(&st.handle_key(crate::app::tests::key(KeyCode::Char('y')))),
        [(
            "loop.control",
            json!({"thread": "t-gamma", "action": "stop"})
        )]
    );
    assert_eq!(
        loop_call(&run_row(&mut st, ActionKind::LoopSkip)),
        [(
            "loop.control",
            json!({"thread": "t-gamma", "action": "skip_to_verify"})
        )]
    );
}

/// On a phone, the Threads screen's cards have the loop line too, fitted to the width: the
/// profile goes before the ledger does.
#[test]
fn a_phones_cards_have_the_loop_line() {
    let mut st = three_threads();
    looping(&mut st, "t-gamma");
    st.view.compact_width = 60;
    draw(&mut st, 40, 20);
    assert!(st.compact());
    st.show_pane(crate::app::CompactPane::Threads);
    st.set_focus(Some("t-gamma".into()));
    let screen = text(&draw(&mut st, 40, 20));
    assert!(
        screen.contains(" REVIEW LOOP  ladder m2 · ledger 1 "),
        "{screen}"
    );
}

/// A review run in flight: the thread is in the background, its activity the reviewer and
/// how long it has run, not a generic task (and no `(1 bg)` for it).
#[test]
fn a_review_run_reads_as_the_reviewer_and_its_time() {
    let mut st = three_threads();
    let mut g = ThreadScript::new("t-gamma");
    g.at(100);
    let mut evs = vec![g.turn_ended(twr_core::event::TurnResult::Completed)];
    let mut task = hub(
        &mut g,
        EventBody::TaskUpdated(TaskUpdated {
            task_id: "loop:l1".into(),
            running: true,
            description: "reviewer gpt-6.1-sol xhigh".into(),
            ..Default::default()
        }),
    );
    task.ts = chrono::Utc::now() - chrono::Duration::seconds(6 * 60 + 5);
    evs.push(task);
    control(&mut st, &evs);
    let screen = read(&mut st);
    // (Wrapped on the card's next row where it is narrow.)
    let at = screen
        .lines()
        .position(|l| l.contains("_ background · reviewer gpt-6.1-sol xhigh"))
        .unwrap_or_else(|| panic!("{screen}"));
    let rest = screen.lines().nth(at + 1).unwrap();
    assert!(rest.contains("· 6m"), "{screen}");
    assert!(!screen.contains("(1 bg)"), "{screen}");
}

/// The setup card: compact on its card, in full (rows aligned) in its popup; its options and
/// `change…`, no dismiss; answered only once read for 600 ms, at the revision shown.
#[test]
fn the_setup_card_compact_and_in_full() {
    let mut st = three_threads();
    raise_setup(&mut st);
    st.set_focus(Some("t-gamma".into()));
    let screen = read(&mut st);
    assert!(
        screen.contains("REVIEW LOOP  first run: choose its models"),
        "{screen}"
    );
    assert!(screen.contains("· 1 warning"), "{screen}");
    assert!(
        screen.contains("1 start and save these settings"),
        "{screen}"
    );
    // The card's compact form is never what an answer is read from.
    assert!(st.view.shown.is_none());
    // The popup: the card in full, its rows aligned.
    st.handle_key(crate::app::tests::key(KeyCode::Enter));
    let screen = text(&draw(&mut st, 160, 40));
    for part in [
        "REVIEW LOOP  gamma: review loop setup (first run: choose its models).",
        "Profile      high · medium tier, then xhigh tier · sweep · 5 verify",
        "Implementer  fake · its default model (the thread's own)",
        "Reviewer     codex · gpt-6.1-sol · medium → xhigh",
        "Warning: reviewer and implementer are the same family",
        " 1 start and save these settings",
        " 2 cancel",
        " change…",
    ] {
        assert!(screen.contains(part), "{part:?}: {screen}");
    }
    // (No dismiss row: [`kinds`] below.)
    assert_eq!(
        kinds(&st),
        [
            ActionKind::Answer(1),
            ActionKind::Answer(2),
            ActionKind::LoopChange,
            ActionKind::Talk,
            ActionKind::Pin,
            ActionKind::Stop,
            ActionKind::Park,
        ]
    );
    // Just drawn: `1` answers nothing yet.
    st.note_drawn(Instant::now());
    assert!(st
        .handle_key(crate::app::tests::key(KeyCode::Char('1')))
        .is_empty());
    assert!(st.view.status.as_deref().unwrap().contains("just changed"));
    // Read: `1` starts and saves, at the revision shown.
    let rev = st.top_attention("t-gamma").unwrap().revision;
    assert!(rev > 0);
    if let Some(s) = st.view.shown.as_mut() {
        s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
    }
    assert_eq!(
        st.handle_key(crate::app::tests::key(KeyCode::Char('1'))),
        [Action::Respond {
            attn_id: "ls1".into(),
            revision: rev,
            decision: "start and save these settings".into(),
        }]
    );
}

/// The setup card in a narrow column and on a short screen: the card cuts its compact form
/// with the others; the popup without room for it opens its detail view, which shows it in
/// full.
#[test]
fn a_setup_card_too_long_for_the_popup_opens_its_detail_view() {
    let mut st = three_threads();
    raise_setup(&mut st);
    st.set_focus(Some("t-gamma".into()));
    draw(&mut st, 120, 16);
    st.handle_key(crate::app::tests::key(KeyCode::Enter));
    draw(&mut st, 120, 16);
    assert_eq!(
        st.view.detail.as_deref(),
        Some("t-gamma"),
        "the detail view"
    );
    let screen = text(&draw(&mut st, 120, 16));
    assert!(screen.contains("too long to show here"), "{screen}");
    assert!(screen.contains(" change…"), "{screen}");
    // Under the popup, the detail view: the card in full.
    st.handle_key(crate::app::tests::key(KeyCode::Esc));
    let screen = text(&draw(&mut st, 120, 30));
    assert!(
        screen.contains("gamma · review loop · Esc returns"),
        "{screen}"
    );
    assert!(screen.contains("Profile      high"), "{screen}");
}

/// In a TUI an agent runs, a loop's cards are shown, never answered: no option rows, no
/// `change…`, and a digit says why.
#[test]
fn an_agents_tui_shows_the_loop_cards_read_only() {
    let mut st = three_threads();
    st.agent = Some("gamma".into());
    raise_setup(&mut st);
    st.set_focus(Some("t-gamma".into()));
    popup_open(&mut st);
    assert_eq!(
        kinds(&st),
        [
            ActionKind::Talk,
            ActionKind::Pin,
            ActionKind::Stop,
            ActionKind::Park
        ]
    );
    assert!(st
        .handle_key(crate::app::tests::key(KeyCode::Char('1')))
        .is_empty());
    let screen = text(&draw(&mut st, 160, 40));
    assert!(
        screen.contains("start and save these settings"),
        "shown: {screen}"
    );
}

/// The stuck card: its badge, `resume` and `stop`, no dismiss.
#[test]
fn the_stuck_card() {
    let mut st = three_threads();
    looping(&mut st, "t-gamma");
    raise_stuck(&mut st);
    st.set_focus(Some("t-gamma".into()));
    let screen = read(&mut st);
    assert!(
        screen.contains("LOOP STUCK  gamma's review loop is stuck"),
        "{screen}"
    );
    popup_open(&mut st);
    assert_eq!(
        kinds(&st),
        [
            ActionKind::Answer(1),
            ActionKind::Answer(2),
            ActionKind::Talk,
            ActionKind::Pin,
            ActionKind::Stop,
            ActionKind::Park,
        ]
    );
    assert!(matches!(
        st.handle_key(crate::app::tests::key(KeyCode::Char('1'))).as_slice(),
        [Action::Respond { attn_id, decision, .. }] if attn_id == "st1" && decision == "resume"
    ));
}

/// The badges in 16 colors keep fills of their own; with none, they are drawn in reverse and
/// read as text.
#[test]
fn the_loop_badges_read_in_every_color_mode() {
    use crate::color::Depth;
    use ratatui::style::{Color, Modifier};
    let mut st = three_threads();
    looping(&mut st, "t-alpha");
    raise_stuck(&mut st);
    let find = |buf: &Buffer, word: &str| -> (Color, Modifier) {
        let rows = lines(buf);
        for (y, row) in rows.iter().enumerate() {
            if let Some(at) = row.find(word) {
                let x = crate::edit::width(&row[..at]) as u16;
                let c = &buf[(x, y as u16)];
                return (c.bg, c.modifier);
            }
        }
        panic!("no {word}: {}", rows.join("\n"));
    };
    st.view.colors = Depth::Basic;
    let buf = draw(&mut st, 160, 40);
    assert_eq!(find(&buf, "REVIEW LOOP").0, Color::LightMagenta);
    assert_eq!(find(&buf, "LOOP STUCK").0, Color::Magenta);
    st.view.colors = Depth::None;
    let buf = draw(&mut st, 160, 40);
    let (bg, m) = find(&buf, "REVIEW LOOP");
    assert_eq!(bg, Color::Reset);
    assert!(m.contains(Modifier::REVERSED));
}

// ---------------------------------------------------------------------------------------------
// /review and /loop
// ---------------------------------------------------------------------------------------------

/// `/review` asks the hub for a loop on the named thread, else the dive's, else the selected
/// card's, with the flags as this run's overrides; `needs_setup` brings the thread's card in
/// front (out of a dive), and nothing answers it.
#[test]
fn review_requests_a_loop_and_needs_setup_selects_the_card() {
    let mut st = three_threads();
    st.view.width = 160;
    assert_eq!(
        loop_call(&type_line(
            &mut st,
            "/review gamma --setup --scope uncommitted --profile low --reviewer codex/gpt-6.1-sol"
        )),
        [(
            "loop.request",
            json!({"thread": "t-gamma", "kind": "review", "setup": true,
                   "scope": "uncommitted",
                   "overrides": {"profile": "low", "reviewer": "codex/gpt-6.1-sol"}})
        )]
    );
    st.dive("t-alpha");
    assert_eq!(
        loop_call(&type_line(&mut st, "/review")),
        [(
            "loop.request",
            json!({"thread": "t-alpha", "kind": "review"})
        )]
    );
    // The card waits: out of the dive, the thread's card selected, nothing answered.
    st.apply_result(
        "loop.request",
        &json!({"asked": {"thread": "t-alpha", "kind": "review"},
                "result": {"status": "needs_setup", "attn_id": "ls9", "revision": 3,
                           "why": "first_run"}}),
    );
    assert_eq!(st.view.dive, None);
    assert_eq!(st.focused(), Some("t-alpha"));
    assert!(st.view.status.as_deref().unwrap().contains("setup card"));
    // Started: what it runs on.
    st.apply_result(
        "loop.request",
        &json!({"asked": {"thread": "t-alpha"},
                "result": {"status": "started",
                           "loop": {"line": "high · scope",
                                    "reviewer": {"runtime": "codex", "model": "gpt-6.1-sol"}}}}),
    );
    assert_eq!(
        st.view.status.as_deref(),
        Some("alpha: review loop started (high · scope); reviewer codex · gpt-6.1-sol")
    );
    // The popup's `review loop` asks the same.
    st.set_focus(Some("t-gamma".into()));
    assert_eq!(
        loop_call(&run_row(&mut st, ActionKind::LoopStart)),
        [(
            "loop.request",
            json!({"thread": "t-gamma", "kind": "review"})
        )]
    );
}

/// `/loop`: its controls on the thread's loop (`stop` asks `y`), `status` in the status bar,
/// `settings` the editor; a thread with no loop says so and sends nothing.
#[test]
fn loop_controls_the_loop_and_says_where_it_is() {
    let mut st = three_threads();
    st.view.width = 160;
    assert!(loop_call(&type_line(&mut st, "/loop gamma pause")).is_empty());
    assert_eq!(
        st.view.status.as_deref(),
        Some("gamma runs no loop (/review starts one)")
    );
    looping(&mut st, "t-gamma");
    assert_eq!(
        loop_call(&type_line(&mut st, "/loop gamma resume")),
        [(
            "loop.control",
            json!({"thread": "t-gamma", "action": "resume"})
        )]
    );
    assert!(loop_call(&type_line(&mut st, "/loop gamma stop")).is_empty());
    assert!(st.view.confirm.is_some());
    st.handle_key(crate::app::tests::key(KeyCode::Char('n')));
    assert!(st.view.confirm.is_none());
    type_line(&mut st, "/loop gamma");
    assert_eq!(
        st.view.status.as_deref(),
        Some(
            "gamma: REVIEW LOOP high · ladder m2 · ledger 1 · scope · ▸ladder · sweep · verify \
             · report · rounds m1 2(1) · m2 ▸"
        )
    );
    assert_eq!(
        loop_call(&type_line(&mut st, "/loop settings --project proj")),
        [("loop.settings.get", json!({"project": "proj"}))]
    );
    let e = st.view.loop_editor.as_ref().unwrap();
    assert_eq!(e.of, Some(EditorFor::Saved));
    assert_eq!(e.project.as_deref(), Some("proj"));
    // The controls' answers.
    st.view.loop_editor = None;
    st.apply_result(
        "loop.control",
        &json!({"asked": {"thread": "t-gamma", "action": "pause"},
                "result": {"thread_name": "gamma", "running": true,
                           "line": "high · ladder m2 · paused"}}),
    );
    assert_eq!(
        st.view.status.as_deref(),
        Some("gamma's review loop paused: high · ladder m2 · paused")
    );
    st.apply_result(
        "loop.control",
        &json!({"asked": {"thread": "t-gamma", "action": "pause"}, "failed": true,
                "code": "conflict", "error": "pause gamma: it is paused already"}),
    );
    assert_eq!(
        st.view.status.as_deref(),
        Some("loop: pause gamma: it is paused already")
    );
}

/// The slash popup completes `--profile` from what `loop.settings.get` said, each with its
/// summary; it is asked once per connection, and again after the saved settings changed.
#[test]
fn the_profiles_come_from_the_hub_once_per_connection() {
    let mut st = three_threads();
    assert_eq!(st.loop_settings_call(), Some(json!({})));
    assert_eq!(st.loop_settings_call(), None);
    st.apply_result(
        "loop.settings.get",
        &json!({"asked": {}, "result": settings_got()}),
    );
    let p = st.loop_profiles();
    assert_eq!(
        p.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["low", "medium", "high", "deep"]
    );
    assert_eq!(
        p[2].1,
        "high · medium tier, then xhigh tier · sweep · 5 verify"
    );
    assert_eq!(p[3].1, "deep · yours ([loops.profiles.deep])");
    let mut h = ThreadScript::new(twr_core::event::HUB_THREAD);
    let saved = hub(&mut h, EventBody::LoopSettingsSaved(Box::default()));
    control(&mut st, &[saved]);
    assert_eq!(st.loop_settings_call(), Some(json!({})));
}

// ---------------------------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------------------------

/// The saved settings' editor: the first card's settings before any save, each preset (one
/// whose runtime is not installed skipped), a named implementer's three rows, the reviewer's
/// runtime cycling past one that is not installed (its model kept only where reported), and
/// Save sending `loop.settings.set` (with `project` for `this project`).
#[test]
fn the_editor_of_the_saved_settings() {
    let mut st = three_threads();
    let asked = st.open_loop_settings(None, false);
    assert_eq!(loop_call(&asked), [("loop.settings.get", json!({}))]);
    st.apply_result(
        "loop.settings.get",
        &json!({"asked": {}, "result": settings_got()}),
    );
    let keys: Vec<RowKey> = editor_rows(&st).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        [
            RowKey::Preset,
            RowKey::Profile,
            RowKey::Implementer,
            RowKey::WorkersModel,
            RowKey::WorkersEffort,
            RowKey::ReviewerRuntime,
            RowKey::ReviewerModel,
            RowKey::ReviewerAccount,
            RowKey::ReviewCommit,
            RowKey::SaveFor,
            RowKey::Save,
            RowKey::Reset,
        ]
    );
    assert_eq!(value(&st, RowKey::Preset), "custom");
    assert_eq!(value(&st, RowKey::Implementer), "the thread's own");
    // The presets: all Grok is skipped (grok-build is not installed).
    change(&mut st, RowKey::Preset, KeyCode::Right);
    assert_eq!(value(&st, RowKey::Preset), "Claude Code + Codex");
    assert_eq!(value(&st, RowKey::Implementer), "named");
    assert_eq!(value(&st, RowKey::ImplRuntime), "claude-code");
    assert_eq!(value(&st, RowKey::ImplModel), "claude-opus-5-5");
    assert_eq!(value(&st, RowKey::ImplEffort), "high");
    change(&mut st, RowKey::Preset, KeyCode::Right);
    assert_eq!(value(&st, RowKey::Preset), "all Claude Code");
    change(&mut st, RowKey::Preset, KeyCode::Right);
    assert_eq!(value(&st, RowKey::Preset), "all Codex");
    // Codex runs no subagents: no workers rows.
    assert!(!editor_rows(&st)
        .iter()
        .any(|(k, _)| *k == RowKey::WorkersModel));
    change(&mut st, RowKey::Preset, KeyCode::Right);
    assert_eq!(value(&st, RowKey::Preset), "Claude Code + Codex");
    // The reviewer: codex → claude-code (its model not reported there: claude's default) →
    // codex (grok-build skipped).
    change(&mut st, RowKey::ReviewerRuntime, KeyCode::Right);
    assert_eq!(value(&st, RowKey::ReviewerRuntime), "claude-code");
    assert_eq!(value(&st, RowKey::ReviewerModel), "claude-opus-5-5");
    let screen = text(&draw(&mut st, 120, 50));
    assert!(
        screen.contains("skipped: grok-build (not installed)"),
        "{screen}"
    );
    change(&mut st, RowKey::ReviewerRuntime, KeyCode::Right);
    assert_eq!(value(&st, RowKey::ReviewerRuntime), "codex");
    assert_eq!(value(&st, RowKey::ReviewerModel), "gpt-6.1-sol");
    assert_eq!(value(&st, RowKey::Preset), "Claude Code + Codex");
    change(&mut st, RowKey::ReviewerAccount, KeyCode::Right);
    assert_eq!(value(&st, RowKey::ReviewerAccount), "work");
    assert_eq!(value(&st, RowKey::Preset), "custom");
    change(&mut st, RowKey::Profile, KeyCode::Left);
    assert_eq!(value(&st, RowKey::Profile), "medium");
    // No project here: all projects only.
    change(&mut st, RowKey::SaveFor, KeyCode::Right);
    assert_eq!(value(&st, RowKey::SaveFor), "all projects");
    // Moving around saved nothing; `s` saves.
    let sent = st.handle_key(crate::app::tests::key(KeyCode::Char('s')));
    let calls = loop_call(&sent);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "loop.settings.set");
    assert!(calls[0].1.get("project").is_none());
    assert_eq!(calls[0].1["settings"]["profile"], "medium");
    assert_eq!(calls[0].1["settings"]["reviewer"]["account"], "work");
    st.apply_result(
        "loop.settings.set",
        &json!({"asked": calls[0].1, "result": {"id": "s1"}}),
    );
    assert!(st.view.loop_editor.is_none());
    assert!(st.view.status.as_deref().unwrap().contains("all projects'"));
    // For one project: `this project` sends it.
    st.open_loop_settings(Some("proj".into()), true);
    st.apply_result(
        "loop.settings.get",
        &json!({"asked": {"project": "proj"}, "result": settings_got()}),
    );
    assert_eq!(value(&st, RowKey::SaveFor), "this project (proj)");
    let sent = change(&mut st, RowKey::Save, KeyCode::Enter);
    let calls = loop_call(&sent);
    assert_eq!(calls[0].0, "loop.settings.set");
    assert_eq!(calls[0].1["project"], "proj");
    // A failed save keeps the editor open, saying why.
    st.apply_result(
        "loop.settings.set",
        &json!({"asked": calls[0].1, "failed": true, "code": "bad_request",
                "error": "these settings do not hold: codex is not logged in"}),
    );
    let e = st.view.loop_editor.as_ref().unwrap();
    assert!(e.note.as_deref().unwrap().contains("not logged in"));
    // Reset asks `y`.
    assert!(change(&mut st, RowKey::Reset, KeyCode::Enter).is_empty());
    assert_eq!(
        loop_call(&st.handle_key(crate::app::tests::key(KeyCode::Char('y')))),
        [("loop.settings.reset", json!({"project": "proj"}))]
    );
    // Esc closes it, saving nothing.
    st.open_loop_settings(None, false);
    assert!(st
        .handle_key(crate::app::tests::key(KeyCode::Esc))
        .is_empty());
    assert!(st.view.loop_editor.is_none());
}

/// A setup card's `change…`: the editor on its draft at the revision loaded; Save sends
/// `loop.setup.update` at that revision; a conflict reloads it and says the card changed; a
/// draft with problems keeps it open, one without closes it back to the card.
#[test]
fn change_edits_the_setup_cards_draft() {
    let mut st = three_threads();
    raise_setup(&mut st);
    st.set_focus(Some("t-gamma".into()));
    let asked = run_row(&mut st, ActionKind::LoopChange);
    assert_eq!(
        loop_call(&asked),
        [
            ("loop.setup.get", json!({"attn_id": "ls1"})),
            ("loop.settings.get", json!({}))
        ]
    );
    let card = |rev: u64, problems: Value| {
        json!({"attn_id": "ls1", "revision": rev, "thread": "t-gamma",
               "setup": {"kind": "review", "first_run": true, "why": "first_run",
                         "settings": {"profile": "high",
                                      "reviewer": {"runtime": "codex", "model": "gpt-6.1-sol"},
                                      "workers": {"model": "claude-opus-5-5", "effort": "high"}},
                         "save_for": "all", "project": "proj",
                         "profile": Profile::builtin("high").unwrap(),
                         "warnings": [], "problems": problems},
               "summary": SETUP_SUMMARY, "options": ["start and save these settings", "cancel"]})
    };
    st.apply_result(
        "loop.setup.get",
        &json!({"asked": {"attn_id": "ls1"}, "result": card(4, json!([]))}),
    );
    st.apply_result(
        "loop.settings.get",
        &json!({"asked": {}, "result": settings_got()}),
    );
    // The thread's runtime (`fake`) has no subagents: no workers rows; no Reset on a card.
    let keys: Vec<RowKey> = editor_rows(&st).into_iter().map(|(k, _)| k).collect();
    assert!(!keys.contains(&RowKey::WorkersModel), "{keys:?}");
    assert!(!keys.contains(&RowKey::Reset), "{keys:?}");
    assert_eq!(value(&st, RowKey::SaveFor), "all projects");
    change(&mut st, RowKey::SaveFor, KeyCode::Right);
    assert_eq!(value(&st, RowKey::SaveFor), "this project (proj)");
    change(&mut st, RowKey::Profile, KeyCode::Right);
    let screen = text(&draw(&mut st, 120, 50));
    assert!(
        screen.contains("gamma's setup card, as raised at revision 4"),
        "{screen}"
    );
    let calls = loop_call(&change(&mut st, RowKey::Save, KeyCode::Enter));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "loop.setup.update");
    assert_eq!(calls[0].1["attn_id"], "ls1");
    assert_eq!(calls[0].1["revision"], 4);
    assert_eq!(calls[0].1["save_for"], "project");
    assert_eq!(calls[0].1["settings"]["profile"], "deep");
    // Raised again since: reloaded, and said.
    st.apply_result(
        "loop.setup.update",
        &json!({"asked": calls[0].1, "failed": true, "code": "conflict",
                "error": "the setup card changed; look again"}),
    );
    assert_eq!(
        loop_call(&st.take_loop_calls()),
        [("loop.setup.get", json!({"attn_id": "ls1"}))]
    );
    assert!(st
        .view
        .loop_editor
        .as_ref()
        .unwrap()
        .note
        .as_deref()
        .unwrap()
        .contains("the card changed"));
    st.apply_result(
        "loop.setup.get",
        &json!({"asked": {"attn_id": "ls1"}, "result": card(5, json!([]))}),
    );
    assert_eq!(
        st.view.loop_editor.as_ref().unwrap().of,
        Some(EditorFor::Card {
            attn_id: "ls1".into(),
            revision: Some(5)
        })
    );
    // The hub's verdict: problems keep it open; none close it, back to the card.
    let calls = loop_call(&st.handle_key(crate::app::tests::key(KeyCode::Char('s'))));
    assert_eq!(calls[0].1["revision"], 5);
    st.apply_result(
        "loop.setup.update",
        &json!({"asked": calls[0].1,
                "result": card(6, json!(["codex isn't logged in"]))}),
    );
    let screen = text(&draw(&mut st, 120, 50));
    assert!(
        screen.contains("change first: codex isn't logged in"),
        "{screen}"
    );
    let calls = loop_call(&st.handle_key(crate::app::tests::key(KeyCode::Char('s'))));
    assert_eq!(calls[0].1["revision"], 6);
    st.apply_result(
        "loop.setup.update",
        &json!({"asked": calls[0].1, "result": card(7, json!([]))}),
    );
    assert!(st.view.loop_editor.is_none());
    assert!(st
        .view
        .status
        .as_deref()
        .unwrap()
        .contains("gamma's setup card shows your settings now"));
}

/// An agent's TUI opens the editor but saves nothing.
#[test]
fn an_agents_tui_saves_no_loop_settings() {
    let mut st = three_threads();
    st.agent = Some("gamma".into());
    st.open_loop_settings(None, false);
    st.apply_result(
        "loop.settings.get",
        &json!({"asked": {}, "result": settings_got()}),
    );
    assert!(st
        .handle_key(crate::app::tests::key(KeyCode::Char('s')))
        .is_empty());
    assert!(st
        .view
        .status
        .as_deref()
        .unwrap()
        .contains("changes no setting"));
}

/// The Settings overlay's Loops row opens the editor over it; Esc returns to the settings.
#[test]
fn the_settings_overlay_has_a_loops_row() {
    let mut st = three_threads();
    st.open_settings();
    let screen = text(&draw(&mut st, 120, 50));
    assert!(screen.contains("Loops"), "{screen}");
    st.handle_key(crate::app::tests::key(KeyCode::End));
    assert_eq!(
        st.view.settings.as_ref().unwrap().key(),
        crate::settings::Key::Loops
    );
    let asked = st.handle_key(crate::app::tests::key(KeyCode::Enter));
    assert_eq!(loop_call(&asked), [("loop.settings.get", json!({}))]);
    assert!(st.view.loop_editor.is_some());
    let screen = text(&draw(&mut st, 120, 50));
    assert!(screen.contains("loop settings"), "{screen}");
    st.handle_key(crate::app::tests::key(KeyCode::Esc));
    assert!(st.view.loop_editor.is_none());
    assert!(st.view.settings.is_some(), "back to the settings");
}

// ---------------------------------------------------------------------------------------------
// A narrow card, a skipped verification, a hub without an agent probe
// ---------------------------------------------------------------------------------------------

fn line_text(l: &ratatui::text::Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The card's loop line degrades in steps: whole; without the profile; short; only then cut,
/// from the end, so where the loop is goes last.
#[test]
fn the_loop_line_degrades_on_a_narrow_card() {
    let mut l = LoopState {
        kind: LoopKind::Review,
        profile: "high".into(),
        phase: twr_core::loops::Phase::Ladder,
        round: Some("x1".into()),
        ledger: 1,
        ..Default::default()
    };
    let at = |l: &LoopState, w: usize| line_text(&crate::loops::card_line(l, w));
    assert_eq!(at(&l, 60), " REVIEW LOOP  high · ladder x1 · ledger 1");
    assert_eq!(at(&l, 40), " REVIEW LOOP  ladder x1 · ledger 1");
    assert_eq!(at(&l, 34), " REVIEW LOOP  ladder x1 · ledger 1");
    assert_eq!(at(&l, 28), " REVIEW LOOP  x1 · ldg 1");
    assert_eq!(at(&l, 18), " REVIEW LOOP  x1 …");
    for w in [40, 34, 28, 18] {
        assert!(crate::edit::width(&at(&l, w)) <= w, "{w}: {:?}", at(&l, w));
    }
    // Paused at 28: short, then cut at the end; the round stays.
    l.paused = true;
    assert_eq!(at(&l, 28), " REVIEW LOOP  x1 · ldg 1 · …");
    // The verification.
    l.paused = false;
    l.phase = twr_core::loops::Phase::Verify;
    l.round = Some("v1".into());
    l.verify = twr_core::loops::VerifyProgress { done: 0, cap: 5 };
    l.ledger = 2;
    assert_eq!(at(&l, 40), " REVIEW LOOP  verify 1/5 · ledger 2");
    assert_eq!(at(&l, 28), " REVIEW LOOP  v1/5 · ldg 2");
    // The sidebar at its default width keeps the ledger.
    let mut st = three_threads();
    looping(&mut st, "t-gamma");
    let screen = read(&mut st);
    assert!(
        screen.contains("REVIEW LOOP  high · ladder m2 · ledger 1")
            || screen.contains("REVIEW LOOP  ladder m2 · ledger 1"),
        "{screen}"
    );
}

/// A loop whose verification was skipped (the last tier's empty round could only be
/// repeated) lists it, and the sweep it never ran, as skipped, not done.
#[test]
fn a_skipped_verification_reads_as_skipped() {
    let (mut m, _) = LoopState::start(&started()).unwrap();
    let now = twr_core::testkit::base_time();
    m.step(Input::GoalWritten { goal: "G".into() }, now)
        .unwrap();
    for round in ["m1", "x1"] {
        m.step(
            Input::ReviewAnswered {
                round: round.into(),
                findings: vec![],
                answer_path: None,
                usage: None,
                unchanged: true,
            },
            now,
        )
        .unwrap();
    }
    assert_eq!(m.phase, twr_core::loops::Phase::Report);
    let rows: Vec<String> = crate::loops::progress_rows(&m, 80)
        .iter()
        .map(line_text)
        .collect();
    assert!(
        rows.contains(&"scope · ladder · sweep skipped · verify skipped · ▸report".to_string()),
        "{rows:?}"
    );
}

/// A hub that does not probe its agent CLIs (`probed: false`) counts every runtime as usable:
/// the editor greys none out and skips none, and says the runtime was not checked.
#[test]
fn the_editor_without_an_agent_probe() {
    let mut st = three_threads();
    st.open_loop_settings(None, false);
    let mut got = settings_got();
    for r in got["runtimes"].as_array_mut().unwrap() {
        r["found"] = json!(true);
        r["login"] = json!(true);
        r["probed"] = json!(false);
    }
    st.apply_result("loop.settings.get", &json!({"asked": {}, "result": got}));
    assert_eq!(value(&st, RowKey::ReviewerRuntime), "codex (not checked)");
    change(&mut st, RowKey::ReviewerRuntime, KeyCode::Right);
    assert_eq!(
        value(&st, RowKey::ReviewerRuntime),
        "grok-build (not checked)"
    );
    let screen = text(&draw(&mut st, 120, 50));
    assert!(!screen.contains("skipped:"), "{screen}");
    assert!(!screen.contains("not installed"), "{screen}");
}
