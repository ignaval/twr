//! Every text cell of the TUI's usual views reads on every terminal (docs/tui.md, "Colors"):
//! whole frames, not hand-picked rows. Each view is drawn through the real draw code in full
//! color, brought down ([`degrade`]) to 16 colors and to none, and read on a dark terminal and
//! a light one, with the model of the loop UI's test ([`super::Variant`], [`super::shown`],
//! [`super::needed`]). A cell that is only a border or a rule (a box-drawing or block
//! character) is left out, unless it is the working icon; a cell in a thread's color on the
//! terminal's own background needs 2:1 on white, as the palette is made to (and 3:1 on black).
//! The views are written as pages with the loop UI's (`loop_ui_shots`).

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use serde_json::json;
use twr_core::event::{AttentionReason, EventBody, TurnResult};
use twr_core::testkit::ThreadScript;

use super::*;
use crate::app::{AppState, CompactPane, Entry, NoticeLevel, Screen};
use crate::ui::tests::{
    account_threads, box_threads_listed, orch_working, prompt, reported_thread, speech_polled,
};

/// A view of the whole screen: every cell of it is measured.
fn whole(name: &'static str, title: &'static str, buf: Buffer) -> Surface {
    Surface {
        name,
        title,
        cells: vec![buf.area],
        crop: buf.area,
        buf,
    }
}

/// The orchestrator's conversation with everything a conversation shows: tool calls (one that
/// failed), a hub notice, each shape of hub update, a `!` command that ended well and one that
/// failed, one still running, a notice of each level, a turn that failed.
fn rich_conversation() -> AppState {
    let mut st = without_worker_requests(with_orch());
    orch_conversation(&mut st);
    let tr = st.transcript.as_mut().unwrap();
    let mut entries = tr.entries.clone();
    let update = |text: &str, outcome: Option<&str>| Entry::HubUpdate {
        text: text.into(),
        user_message: None,
        attn_id: None,
        about: None,
        outcome: outcome.map(str::to_string),
        seq: 0,
        resolution: false,
        routine: None,
    };
    let shell =
        |command: &str, running: bool, outcome: Option<&str>, exit: Option<i64>| Entry::Shell {
            seq: 0,
            job: format!("j-{command}"),
            command: command.into(),
            cwd: "/tmp/proj".into(),
            share: false,
            running,
            outcome: outcome.map(str::to_string),
            exit,
            signal: None,
            duration_ms: Some(1200),
            output: "compiling twr\nwarning: unused\n   Finished dev".into(),
            read: true,
        };
    entries.extend([
        update("beta needs permission: Bash: rm -rf target", None),
        update(
            "beta needs permission: Bash: rm -rf target",
            Some("answered: allow"),
        ),
        update("alpha asks: which branch?", Some("answered: main")),
        update("alpha needs permission: Bash: curl", Some("answered: deny")),
        update("gamma needs review: the diff", None),
        update("gamma is stalled: no output", None),
        update("gamma failed: the build broke", None),
        update("gamma finished (success): all green", None),
        update("alpha finished (failure): the tests fail", None),
        update("beta finished (partial): half of it", None),
        update("Watching gamma: its tests", None),
        update("Stopped watching gamma (w1).", Some("closed")),
        shell("cargo build", false, Some("exited"), Some(0)),
        shell("cargo test", false, Some("exited"), Some(101)),
        shell("sleep 9", false, Some("cancelled"), None),
        shell("make", true, None, None),
        Entry::Notice {
            seq: 0,
            level: NoticeLevel::Info,
            text: "the hub restarted".into(),
        },
        Entry::Notice {
            seq: 0,
            level: NoticeLevel::Attention,
            text: "beta needs you".into(),
        },
        Entry::Notice {
            seq: 0,
            level: NoticeLevel::Error,
            text: "gamma crashed".into(),
        },
        Entry::Tool {
            call_id: "x1".into(),
            tool: "Bash".into(),
            summary: "cargo test".into(),
            ok: Some(true),
            thread: None,
        },
        Entry::Tool {
            call_id: "x2".into(),
            tool: "Bash".into(),
            summary: "cargo clippy".into(),
            ok: Some(false),
            thread: None,
        },
        Entry::Tool {
            call_id: "x3".into(),
            tool: "Read".into(),
            summary: "src/lib.rs".into(),
            ok: None,
            thread: None,
        },
        Entry::TurnEnd {
            result: TurnResult::Error,
            duration_ms: Some(3000),
        },
    ]);
    tr.set_entries(entries);
    st
}

/// A workspace sidebar with several workspaces (one current, things waiting on you in two).
fn workspaces() -> AppState {
    let mut st = crate::sidebar::tests::in_workspaces("client-x");
    st.view.compact_width = 0;
    st
}

/// Every usual view, whole.
pub(super) fn frames() -> Vec<Surface> {
    let mut out = Vec::new();
    // The stack: cards, one selected; every state; the parked note.
    let mut st = three_threads();
    st.set_focus(Some("t-gamma".into()));
    out.push(whole(
        "f-stack",
        "The stack: three cards, one selected",
        full(&mut st, 160, 40),
    ));
    let mut st = all_states();
    st.view.show_parked = true;
    out.push(whole(
        "f-states",
        "The stack: a thread in each state",
        full(&mut st, 160, 60),
    ));
    let mut st = five_threads();
    out.push(whole(
        "f-five",
        "The stack: five threads, git lines",
        full(&mut st, 160, 60),
    ));
    // A dive, another card selected.
    let mut st = three_threads();
    st.dive("t-beta");
    st.set_focus(Some("t-gamma".into()));
    out.push(whole("f-dive", "A dive into beta", full(&mut st, 160, 40)));
    // The grid.
    let mut st = all_states();
    st.view.screen = Screen::Grid;
    out.push(whole("f-grid", "The grid", full(&mut st, 160, 50)));
    // The conversation: tool calls, hub updates, shell commands, notices.
    let mut st = rich_conversation();
    out.push(whole(
        "f-conversation",
        "The conversation folded",
        full(&mut st, 160, 70),
    ));
    st.view.expand_all = true;
    out.push(whole(
        "f-conversation-open",
        "The conversation open (ctrl+o)",
        full(&mut st, 160, 90),
    ));
    // The usage windows near their limits, a busy machine.
    let mut st = rich_conversation();
    usage(&mut st, 80.0, 95.0);
    let psi = |v: f64| format!("some avg10={v:.2} avg60=0 avg300=0 total=0\n");
    st.load = twr_core::load::Load::from_proc(Some(&psi(52.0)), Some(&psi(0.0)), None, None, 4);
    out.push(whole(
        "f-usage",
        "The status bar: usage near its limits, a busy machine",
        full(&mut st, 200, 30),
    ));
    let mut st = rich_conversation();
    usage(&mut st, 30.0, 50.0);
    st.load = twr_core::load::Load::from_proc(Some(&psi(97.0)), Some(&psi(31.0)), None, None, 4);
    out.push(whole(
        "f-usage-calm",
        "The status bar: usage, a saturated machine",
        full(&mut st, 200, 30),
    ));
    // A shell line typed.
    let mut st = rich_conversation();
    for c in "!ls".chars() {
        st.handle_key(key(KeyCode::Char(c)));
    }
    out.push(whole(
        "f-shell-input",
        "A ! line in the input box",
        full(&mut st, 160, 40),
    ));
    // The commands' completions.
    let mut st = rich_conversation();
    st.handle_key(key(KeyCode::Char('/')));
    out.push(whole(
        "f-completions",
        "The commands' completions",
        full(&mut st, 160, 40),
    ));
    let mut st = rich_conversation();
    for c in "@".chars() {
        st.handle_key(key(KeyCode::Char(c)));
    }
    out.push(whole(
        "f-mentions",
        "An @ mention's completions",
        full(&mut st, 160, 40),
    ));
    // The action popup, filtered.
    let mut st = three_threads();
    st.set_focus(Some("t-beta".into()));
    popup_open(&mut st);
    out.push(whole(
        "f-popup",
        "A card's action popup",
        full(&mut st, 160, 40),
    ));
    st.handle_key(key(KeyCode::Char('a')));
    out.push(whole(
        "f-popup-filter",
        "A card's action popup, filtered",
        full(&mut st, 160, 40),
    ));
    // The sidebar with workspaces, the tab strip with it hidden.
    let mut st = workspaces();
    out.push(whole(
        "f-sidebar",
        "The sidebar: several workspaces",
        full(&mut st, 160, 50),
    ));
    st.set_sidebar(false);
    out.push(whole(
        "f-tabs",
        "The tab strip: the sidebar hidden",
        full(&mut st, 160, 40),
    ));
    let mut st = workspaces();
    st.view.screen = Screen::Grid;
    out.push(whole(
        "f-sidebar-grid",
        "The sidebar beside the grid",
        full(&mut st, 160, 50),
    ));
    // The phone: its screens, a popup, the workspaces.
    for (name, title, pane) in [
        ("f-phone-threads", "A phone: Threads", CompactPane::Threads),
        (
            "f-phone-chat",
            "A phone: the conversation",
            CompactPane::Chat,
        ),
    ] {
        let mut st = rich_conversation();
        st.view.compact_width = 60;
        st.show_pane(pane);
        out.push(whole(name, title, full(&mut st, 40, 60)));
    }
    let mut st = three_threads();
    st.view.compact_width = 60;
    st.show_pane(CompactPane::Threads);
    full(&mut st, 40, 40);
    st.handle_key(key(KeyCode::Enter));
    out.push(whole(
        "f-phone-popup",
        "A phone: a card's popup",
        full(&mut st, 40, 40),
    ));
    let mut st = crate::sidebar::tests::in_workspaces("default");
    st.show_pane(CompactPane::Threads);
    out.push(whole(
        "f-phone-workspaces",
        "A phone: the workspaces and the tab bar",
        full(&mut st, 40, 60),
    ));
    let mut st = crate::sidebar::tests::in_workspaces("default");
    st.show_pane(CompactPane::Chat);
    st.handle_key(key(KeyCode::Char('/')));
    out.push(whole(
        "f-phone-completions",
        "A phone: the completions",
        full(&mut st, 40, 40),
    ));
    // The settings, the orchestrator picker, the model picker.
    let mut st = three_threads();
    st.open_settings();
    out.push(whole("f-settings", "The settings", full(&mut st, 120, 50)));
    let mut st = three_threads();
    st.view.picker = crate::app::Picker::from_show(
        &json!({
            "thread": null, "retired": 0,
            "configured": {"runtime": "codex", "explicit": false},
            "runtimes": ["claude-code", "codex", "gemini"],
            "installed": {"claude-code": true, "codex": false},
        }),
        &Default::default(),
    );
    out.push(whole(
        "f-picker",
        "The orchestrator picker",
        full(&mut st, 120, 40),
    ));
    // The help, the trash, the decisions.
    let mut st = three_threads();
    st.view.help = true;
    out.push(whole("f-help", "The help", full(&mut st, 160, 60)));
    let mut st = three_threads();
    st.view.trash = Some(json!({
        "items": [{"short": "t-old1", "thread_name": "old", "project": "proj", "box": "local",
                   "size": "2 MB", "deleted_at": "2026-11-01T10:00:00Z",
                   "purge_at": "2026-12-01T10:00:00Z", "losses": "its worktree"}],
        "unavailable": [{"box": "gpu", "error": "offline"}],
        "message": "1 deleted thread"
    }));
    out.push(whole("f-trash", "The trash", full(&mut st, 160, 40)));
    let mut st = with_decisions();
    st.view.screen = Screen::Decisions;
    out.push(whole(
        "f-decisions",
        "The decisions",
        full(&mut st, 160, 40),
    ));
    // A confirm in the status bar, the hub lost.
    let mut st = with_orch();
    type_line(&mut st, "/clear");
    out.push(whole(
        "f-confirm",
        "A confirm in the status bar",
        full(&mut st, 160, 40),
    ));
    let mut st = three_threads();
    st.view.hub_lost = Some(crate::app::HubLost {
        text: "the hub is gone; reconnecting".into(),
        refused: false,
    });
    out.push(whole("f-hub-lost", "The hub lost", full(&mut st, 160, 40)));
    // A status message, a thread's card with each kind of request.
    let mut st = three_threads();
    let mut g = ThreadScript::new("t-gamma");
    g.at(700);
    let ev = g.raise("q1", AttentionReason::Question);
    control(&mut st, &[ev]);
    st.set_status("something to say".into(), false, std::time::Instant::now());
    out.push(whole(
        "f-requests",
        "Cards with each request, a status message",
        full(&mut st, 160, 50),
    ));
    out.extend(more_frames());
    out
}

/// A drag over the whole of `pane` (the conversation by default): the mouse selection.
fn select_all(st: &mut AppState, w: u16, h: u16) {
    full(st, w, h);
    let pane = st
        .view
        .panes
        .get(&crate::select::PaneId::Transcript)
        .expect("a conversation")
        .clone();
    let a = pane.area;
    let mut sel = crate::select::Selection::start(&pane, a.x, a.y);
    sel.extend(a.right() - 1, a.bottom() - 1);
    st.view.selection = Some(sel);
}

/// The views with something selected or open over them, and the rarer ones.
fn more_frames() -> Vec<Surface> {
    let mut out = Vec::new();
    // The hub's card (`twr asks`), selected; the orchestrator's banner selected.
    let mut st = with_orch();
    let mut h = ThreadScript::new(twr_core::event::HUB_THREAD);
    h.at(90);
    control(&mut st, &[h.raise("hq9", AttentionReason::Question)]);
    out.push(whole(
        "f-hub-card",
        "The hub's card",
        full(&mut st, 160, 50),
    ));
    st.set_focus(Some(twr_core::event::HUB_THREAD.into()));
    out.push(whole(
        "f-hub-card-selected",
        "The hub's card selected",
        full(&mut st, 160, 50),
    ));
    // Requests in the conversation, one selected.
    let mut st = with_orch();
    out.push(whole(
        "f-requests-boxed",
        "Requests in the conversation",
        full(&mut st, 160, 60),
    ));
    st.view.request = Some("pm1".into());
    st.view.main_focus = true;
    out.push(whole(
        "f-request-selected",
        "A request selected in the conversation",
        full(&mut st, 160, 60),
    ));
    // A card pinned, a card with a report; its detail.
    let mut st = reported_thread();
    st.view.pins.insert("t-alpha".into());
    out.push(whole(
        "f-pinned",
        "A pinned card, a reported one",
        full(&mut st, 160, 50),
    ));
    // Threads on a box in each state of it.
    for (name, state) in [
        ("f-box-switching", "switching"),
        ("f-box-offline", "offline"),
    ] {
        let mut st = box_threads_listed(Some(crate::app::tests::info_box("gpu", state)), true);
        out.push(whole(
            name,
            "Threads on a remote box",
            full(&mut st, 160, 40),
        ));
    }
    // Accounts named on the cards.
    let mut st = account_threads(Some(crate::app::tests::fake_accounts(true)));
    out.push(whole(
        "f-accounts",
        "Cards naming their accounts",
        full(&mut st, 160, 50),
    ));
    // The queue, one selected.
    let mut st = with_orch();
    st.queue_due(std::time::Instant::now());
    st.apply_result(
        "thread.queue",
        &json!({"thread": "t-orch", "thread_name": "orchestrator", "total": 5, "held": false,
                "items": [
                    {"ref": 7, "prompt": "fix the tests too", "truncated": false,
                     "from": "user", "mine": true},
                    {"ref": 8, "prompt": "check the api", "truncated": false,
                     "from": "orchestrator", "mine": false}]}),
    );
    out.push(whole("f-queue", "Queued messages", full(&mut st, 120, 30)));
    st.view.queued = Some(7);
    out.push(whole(
        "f-queue-selected",
        "A queued message selected",
        full(&mut st, 120, 30),
    ));
    // A message read aloud.
    let text = |seq: u64, t: &str| Entry::Text {
        seq,
        text: t.into(),
        open: false,
        final_seq: Some(seq + 1),
    };
    let mut st = orch_working(vec![
        prompt("hi"),
        text(3, "first reply"),
        prompt("more"),
        text(6, "second reply"),
    ]);
    speech_polled(
        &mut st,
        twr_core::speech::SpeechStateParams {
            playing: crate::speech::tests::playing("t-orch", 7),
            ..Default::default()
        },
    );
    out.push(whole(
        "f-read-aloud",
        "A message read aloud, the orchestrator working",
        full(&mut st, 120, 30),
    ));
    // The mouse selection over the conversation's colors.
    let mut st = rich_conversation();
    select_all(&mut st, 160, 70);
    out.push(whole(
        "f-mouse-selection",
        "The mouse selection over the conversation",
        full(&mut st, 160, 70),
    ));
    // A long block selected.
    let mut st = rich_conversation();
    let long: String = (0..40)
        .map(|i| format!("line {i} of a long reply\n"))
        .collect();
    let tr = st.transcript.as_mut().unwrap();
    let mut entries = tr.entries.clone();
    entries.push(text(900, &long));
    tr.set_entries(entries);
    st.view.block = Some("block:t-orch:text:900:0".into());
    out.push(whole(
        "f-long-block",
        "A long block selected",
        full(&mut st, 120, 40),
    ));
    // A dive into the orchestrator (the hub's magenta).
    let mut st = with_orch();
    st.dive("t-orch");
    out.push(whole(
        "f-dive-orch",
        "A dive into the orchestrator",
        full(&mut st, 160, 40),
    ));
    // The phone's Needs you screen and a selected card there.
    let mut st = with_orch();
    st.view.compact_width = 60;
    st.show_pane(CompactPane::Needs);
    out.push(whole(
        "f-phone-needs",
        "A phone: Needs you",
        full(&mut st, 40, 50),
    ));
    let mut st = three_threads();
    st.view.compact_width = 60;
    st.show_pane(CompactPane::Threads);
    st.handle_key(key(KeyCode::Down));
    out.push(whole(
        "f-phone-selected",
        "A phone: a card selected",
        full(&mut st, 40, 50),
    ));
    // The model picker, a new workspace, the sidebar with a workspace selected.
    let mut st = three_threads();
    type_line(&mut st, "/model alpha");
    out.push(whole(
        "f-model-picker",
        "The model picker",
        full(&mut st, 120, 40),
    ));
    let mut st = workspaces();
    st.view.new_ws = Some("scratch".into());
    out.push(whole(
        "f-new-workspace",
        "A new workspace",
        full(&mut st, 160, 40),
    ));
    let mut st = workspaces();
    st.set_focus(None);
    st.view.ws_sel = Some(crate::app::WsRow::Workspace("client-x".into()));
    out.push(whole(
        "f-sidebar-selected",
        "The sidebar, a waiting workspace selected",
        full(&mut st, 160, 50),
    ));
    // The decisions view filtered.
    let mut st = with_decisions();
    st.view.screen = Screen::Decisions;
    st.handle_key(key(KeyCode::Char('f')));
    out.push(whole(
        "f-decisions-filter",
        "The decisions filtered",
        full(&mut st, 160, 40),
    ));
    // The phone's confirm, an empty workspace, the runner panel.
    let mut st = with_orch();
    st.view.compact_width = 60;
    st.show_pane(CompactPane::Chat);
    type_line(&mut st, "/clear");
    out.push(whole(
        "f-phone-confirm",
        "A phone: a confirm",
        full(&mut st, 40, 40),
    ));
    let mut st = AppState::new();
    out.push(whole("f-empty", "No threads", full(&mut st, 120, 30)));
    // A worker orchestrator's card.
    let mut st = worker_orch_and_long_card(30);
    out.push(whole(
        "f-worker-orch",
        "A worker orchestrator's card, a long card",
        full(&mut st, 160, 50),
    ));
    // The loop editor and a setup card, whole.
    let mut st = three_threads();
    st.open_loop_settings(None, false);
    st.apply_result(
        "loop.settings.get",
        &json!({"asked": {}, "result": crate::loops_tests::settings_got()}),
    );
    out.push(whole(
        "f-loop-editor",
        "The loop editor",
        full(&mut st, 120, 50),
    ));
    // The grid, a tile selected; a stalled notice; a failed turn.
    let mut st = all_states();
    st.view.screen = Screen::Grid;
    st.set_focus(Some("t-wait".into()));
    out.push(whole(
        "f-grid-selected",
        "The grid, a tile selected",
        full(&mut st, 160, 50),
    ));
    let mut st = three_threads();
    let mut g = ThreadScript::new("t-gamma");
    g.at(700);
    control(
        &mut st,
        &[
            g.raise("s1", AttentionReason::Stalled),
            g.raise("e1", AttentionReason::Error),
        ],
    );
    out.push(whole(
        "f-stalled",
        "A stalled notice and an error",
        full(&mut st, 160, 50),
    ));
    // Focus mode, the sidebar hidden (the status bar's notes), the hub refusing this TUI.
    let mut st = all_states();
    st.view.focus_mode = true;
    st.set_sidebar(false);
    out.push(whole(
        "f-focus",
        "Focus mode, the sidebar hidden",
        full(&mut st, 160, 40),
    ));
    let mut st = three_threads();
    st.view.hub_lost = Some(crate::app::HubLost {
        text: "this hub refuses this TUI: start it again".into(),
        refused: true,
    });
    out.push(whole(
        "f-hub-refused",
        "The hub refusing this TUI",
        full(&mut st, 160, 40),
    ));
    // The settings scrolled to their end, a dive into a thread on a switching box.
    let mut st = three_threads();
    st.open_settings();
    st.handle_key(key(KeyCode::End));
    out.push(whole(
        "f-settings-end",
        "The settings, the last row selected",
        full(&mut st, 120, 50),
    ));
    let mut st = box_threads_listed(Some(crate::app::tests::info_box("gpu", "switching")), true);
    st.dive("t-job");
    out.push(whole(
        "f-dive-box",
        "A dive into a thread on a switching box",
        full(&mut st, 160, 40),
    ));
    out
}

/// What the hub knows of the subscriptions' usage: Claude's 5-hour and weekly windows at `five`
/// and `week` used, Codex's weekly at 42%.
fn usage(st: &mut AppState, five: f64, week: f64) {
    use twr_core::event::{AccountUsage, Event, Source, HUB_THREAD};
    use twr_core::usage::UsageWindow;
    let now = chrono::Utc::now();
    let win = |id: &str, pct: f64, hours: i64| UsageWindow {
        window: id.into(),
        used_pct: pct,
        resets_at: Some(now + chrono::TimeDelta::hours(hours)),
        minutes: None,
        observed_at: None,
    };
    for (runtime, windows, ago) in [
        (
            "claude-code",
            vec![win("five_hour", five, 3), win("seven_day", week, 100)],
            1,
        ),
        ("codex", vec![win("seven_day", 42.0, 100)], 12),
    ] {
        let mut ev = Event::new(
            HUB_THREAD,
            1,
            None,
            Source::Hub,
            EventBody::AccountUsage(AccountUsage {
                runtime: runtime.into(),
                windows,
                source: "event".into(),
                ..Default::default()
            }),
        );
        ev.ts = now - chrono::TimeDelta::minutes(ago) - chrono::TimeDelta::seconds(5);
        st.apply_control(&ev);
    }
}

/// Every text cell of every usual view reads on every terminal: each failing color, with
/// the views it is in and where.
#[test]
fn every_text_cell_of_every_view_reads_on_every_terminal() {
    let mut fails = std::collections::BTreeMap::<String, (Vec<&str>, Vec<String>)>::new();
    for s in frames() {
        for v in VARIANTS {
            for what in read(&s, v).short {
                let (color, place) = what.split_once(" in ").unwrap_or((&what, ""));
                let e = fails.entry(format!("{} {color}", v.name)).or_default();
                if !e.0.contains(&s.name) {
                    e.0.push(s.name);
                }
                if e.1.len() < 4 && !e.1.contains(&place.to_string()) {
                    e.1.push(place.to_string());
                }
            }
        }
    }
    let lines: Vec<String> = fails
        .iter()
        .map(|(what, (views, places))| {
            format!("{what} [{}] {}", views.join(" "), places.join(" | "))
        })
        .collect();
    assert!(
        lines.is_empty(),
        "{} short:\n{}",
        lines.len(),
        lines.join("\n")
    );
}
