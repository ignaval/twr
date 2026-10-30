//! Long cards on a phone: an item, a report or a question waiting for `y` taller than the
//! screen is answered by touch (taps, and the wheel a swipe sends) and by keys from the rows the
//! screen first shows, upright, with the keyboard open, or on its side, its whole text there to
//! scroll through when you want to; the answer sent is the option tapped, on the item and
//! revision on screen, never one moved under the finger. Driven as the IO loop drives the TUI:
//! a frame before and after each input.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use twr_core::event::AttentionReason::{self, *};
use twr_core::event::EventBody;
use twr_core::testkit::ThreadScript;

use crate::app::tests::*;
use crate::app::{Action, ActionKind, AppState, CompactPane, CARD_SETTLE};
use crate::select::{PaneId, Scrolled, Target};

/// A phone held upright; with its keyboard open; on its side, keyboard open (wider than
/// `compact_width`: compact by its height).
const UPRIGHT: (u16, u16) = (40, 20);
const KEYBOARD: (u16, u16) = (40, 11);
const LANDSCAPE: (u16, u16) = (80, 9);

/// The fixture with `[tui] compact_width` at its default (60): 40 columns is a phone's, and
/// 80 x 9 one on its side by its height alone.
fn phone() -> AppState {
    let mut st = three_threads();
    st.view.compact_width = twr_core::config::TuiConfig::default().compact_width;
    st
}

/// Draws at `size` as the IO loop does, noting what it showed.
fn frame(st: &mut AppState, (w, h): (u16, u16)) -> Buffer {
    crate::ui::draw_offscreen(st, w, h, Instant::now())
}

fn lines(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (0..a.height)
        .map(|y| (0..a.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn screen(st: &mut AppState, size: (u16, u16)) -> String {
    lines(&frame(st, size)).join("\n")
}

/// What is on screen has been there long enough to be read.
fn settle(st: &mut AppState) {
    let then = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
    if let Some(s) = st.view.shown.as_mut() {
        s.since = then;
    }
    if let Some((_, since)) = st.view.confirm_seen.as_mut() {
        *since = then;
    }
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

/// A finger's tap at `(x, y)`: down and up, a frame before each and after.
fn tap_at(st: &mut AppState, size: (u16, u16), x: u16, y: u16) -> Vec<Action> {
    let left = MouseButton::Left;
    let screen = frame(st, size);
    let mut out = st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
    let screen = frame(st, size);
    out.extend(st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen));
    frame(st, size);
    out
}

/// The rects the last frame drew control `t` over.
fn rects(st: &AppState, t: &Target) -> Vec<Rect> {
    st.view
        .panes
        .targets
        .iter()
        .filter(|(_, x)| x == t)
        .map(|(r, _)| *r)
        .collect()
}

/// A tap on control `t` as drawn now (its first rect's middle); `None` when not drawn.
fn tap(st: &mut AppState, size: (u16, u16), t: &Target) -> Option<Vec<Action>> {
    frame(st, size);
    let r = *rects(st, t).first()?;
    Some(tap_at(st, size, r.x + r.width / 2, r.y + r.height / 2))
}

/// A swipe's wheel step over `(x, y)`.
fn wheel(st: &mut AppState, size: (u16, u16), x: u16, y: u16, down: bool) {
    let screen = frame(st, size);
    let kind = if down {
        MouseEventKind::ScrollDown
    } else {
        MouseEventKind::ScrollUp
    };
    st.handle_mouse(mouse(kind, x, y), &screen);
    frame(st, size);
}

/// The action popup's inner area as last drawn.
fn popup(st: &AppState) -> Rect {
    st.view
        .panes
        .get(&PaneId::Popup)
        .map(|p| p.area)
        .expect("the action popup")
}

/// Swipes over the popup's first row of text until no `more lines` is left (at most `max`
/// steps); returns every row the popup showed meanwhile, trimmed.
fn read_to_end(st: &mut AppState, size: (u16, u16), max: usize) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    for _ in 0..max {
        let buf = frame(st, size);
        let p = popup(st);
        for y in p.top()..p.bottom() {
            let row: String = (p.left()..p.right())
                .map(|x| buf[(x, y)].symbol())
                .collect();
            seen.insert(row.trim().to_string());
        }
        if !lines(&buf).iter().any(|l| l.contains("more lines")) {
            break;
        }
        wheel(st, size, p.x + 1, p.y, true);
    }
    seen
}

fn responded(out: &[Action]) -> Option<(&str, u64, &str)> {
    match out {
        [Action::Respond {
            attn_id,
            revision,
            decision,
        }] => Some((attn_id.as_str(), *revision, decision.as_str())),
        _ => None,
    }
}

fn revision_of(st: &AppState, id: &str) -> u64 {
    st.attention.get(id).expect("the item").revision
}

/// `delta` asking (`reason`) item `id`: `rows` lines of text (accents and a check mark in
/// each) and `options` options.
fn raise_long(st: &mut AppState, reason: AttentionReason, id: &str, rows: usize, options: usize) {
    let mut d = ThreadScript::new("t-delta");
    d.at(70);
    let mut q = d.raise(id, reason);
    if let EventBody::AttentionRaised(p) = &mut q.body {
        p.summary = (0..rows)
            .map(|i| format!("line {i:02}: carefully worded détails ✓ of what is asked"))
            .collect::<Vec<_>>()
            .join("\n");
        p.options = (1..=options).map(|n| format!("choice {n:02}")).collect();
    }
    control(st, &[d.created("delta"), d.turn_started("ask"), q]);
}

/// Opens delta's action popup from Needs you by touch: its tab, its row, its row again.
fn open_from_needs(st: &mut AppState, size: (u16, u16)) {
    frame(st, size);
    tap(st, size, &Target::Screen(CompactPane::Needs)).expect("the Needs you tab");
    assert_eq!(st.view.compact_pane, CompactPane::Needs);
    let row = |st: &mut AppState| {
        frame(st, size);
        st.view
            .panes
            .get(&PaneId::Thread("t-delta".into()))
            .map(|p| p.area)
            .expect("delta's row")
    };
    // Scrolled to it (the wheel over the list) where the list is longer than the screen.
    for _ in 0..10 {
        frame(st, size);
        if st
            .view
            .panes
            .get(&PaneId::Thread("t-delta".into()))
            .is_some()
        {
            break;
        }
        let list = st.view.panes.list.expect("the Needs you list").area;
        wheel(st, size, list.x + 1, list.y, true);
    }
    for _ in 0..3 {
        if st.view.action.is_some() {
            break;
        }
        let r = row(st);
        tap_at(st, size, r.x + r.width / 2, r.y);
    }
    assert!(st.view.action.is_some(), "its popup opened");
}

/// Taps option `n`'s row, its window of rows scrolled to it first (the wheel over them),
/// then taps it again; returns what the second tap did.
fn answer_by_taps(st: &mut AppState, size: (u16, u16), n: usize) -> Vec<Action> {
    let row = Target::Row(ActionKind::Answer(n));
    for _ in 0..60 {
        frame(st, size);
        if !rects(st, &row).is_empty() {
            break;
        }
        // Under a higher option, down; else up. Over the popup's last row: its rows.
        let below = st.view.panes.popup_rows.as_ref().is_some_and(|(_, k)| {
            k.iter()
                .any(|k| matches!(k, ActionKind::Answer(m) if *m > n))
        });
        let p = popup(st);
        wheel(st, size, p.x + 1, p.bottom() - 1, !below);
    }
    settle(st);
    let first = tap(st, size, &row).unwrap_or_else(|| panic!("option {n} drawn"));
    assert!(
        responded(&first).is_none(),
        "one tap only picks it: {first:?}"
    );
    settle(st);
    tap(st, size, &row).unwrap_or_else(|| panic!("option {n} still drawn"))
}

/// Every control the last frame drew lies on its text: an option's row starts with its
/// number, a text's cue says what it pages; none over another; an option, a tab, `yes` and
/// `no` two rows tall, a cue and `✕` one row.
fn targets_agree(st: &AppState, buf: &Buffer, what: &str) {
    let text = |r: &Rect| -> String {
        (r.top()..r.bottom())
            .map(|y| {
                (r.left()..r.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("|")
    };
    let targets = &st.view.panes.targets;
    for (k, (a, t)) in targets.iter().enumerate() {
        match t {
            Target::Row(ActionKind::Answer(n)) => {
                assert!(a.height >= 2, "{what}: {t:?} {a:?}");
                let s = text(a);
                let start = s.trim_start_matches([' ', '│']);
                assert!(
                    start.starts_with(&format!("{n} ")) || start.starts_with(&format!("{n}. ")),
                    "{what}: {t:?} over {s:?}"
                );
            }
            Target::Row(_) | Target::Screen(_) | Target::Yes | Target::No => {
                assert!(a.height >= 2, "{what}: {t:?} {a:?}");
            }
            Target::More(_) => {
                assert_eq!(a.height, 1, "{what}: {t:?}");
                let s = text(a);
                assert!(
                    s.contains("more") || s.contains("above") || s.contains("top"),
                    "{what}: {t:?} over {s:?}"
                );
            }
            Target::Close => assert_eq!(a.height, 1, "{what}: {t:?}"),
            _ => {}
        }
        for (b, u) in &targets[k + 1..] {
            assert!(
                t == u || !a.intersects(*b),
                "{what}: {t:?} {a:?} over {u:?} {b:?}"
            );
        }
    }
}

/// Where the action popup's text is scrolled to (its first row drawn), and how many rows it has.
fn text_at(st: &AppState) -> (usize, usize) {
    let w = st.view.panes.sheet.as_ref().expect("the popup's text");
    (w.start, w.rows)
}

/// From Needs you, by touch alone, upright, keyboard open and on its side: a long question is
/// answered from the first rows it shows, its text never scrolled (more of it below), with the
/// option tapped twice, for that item and revision; the controls lie on their text.
#[test]
fn a_long_question_is_answered_by_touch_from_its_first_rows() {
    for size in [UPRIGHT, KEYBOARD, LANDSCAPE] {
        let mut st = phone();
        raise_long(&mut st, Question, "q1", 30, 4);
        open_from_needs(&mut st, size);
        let buf = frame(&mut st, size);
        targets_agree(&st, &buf, &format!("{size:?}"));
        assert!(
            lines(&buf).iter().any(|l| l.contains("more lines")),
            "{size:?}"
        );
        assert_eq!(text_at(&st).0, 0);
        let out = answer_by_taps(&mut st, size, 3);
        assert_eq!(st.view.sheet_view.scroll, 0, "{size:?}: never scrolled");
        assert_eq!(
            responded(&out),
            Some(("q1", revision_of(&st, "q1"), "choice 03")),
            "{size:?}: {out:?}\n{}",
            screen(&mut st, size)
        );
    }
}

/// The whole text is there to read when you want to: swiped through, every line comes on
/// screen, and then (scrolled to its end, or back to its top by its cue) an option answers as
/// it did before.
#[test]
fn the_whole_text_scrolls_into_view_when_wanted() {
    for size in [UPRIGHT, KEYBOARD, LANDSCAPE] {
        let mut st = phone();
        raise_long(&mut st, Question, "q1", 30, 4);
        open_from_needs(&mut st, size);
        let seen = read_to_end(&mut st, size, 200);
        for i in 0..30 {
            let line = format!("line {i:02}:");
            assert!(
                seen.iter().any(|r| r.contains(&line)),
                "{size:?}: {line} never on screen: {seen:?}"
            );
        }
        let (at, rows) = text_at(&st);
        assert!(at > 0 && at < rows, "{size:?}");
        let out = tap(&mut st, size, &Target::More(Scrolled::Sheet)).expect("its cue");
        assert!(out.is_empty() && st.view.action.is_some());
        assert_eq!(text_at(&st).0, 0, "{size:?}: back at its top");
        let out = answer_by_taps(&mut st, size, 2);
        assert_eq!(responded(&out).map(|r| r.2), Some("choice 02"), "{size:?}");
    }
}

/// A permission whose command wraps over many rows, and a hung turn's card with choices:
/// answered from their first rows, the rest of the command there to scroll to.
#[test]
fn a_long_permission_and_a_hung_card_are_answered_by_touch() {
    for size in [UPRIGHT, KEYBOARD, LANDSCAPE] {
        let mut st = phone();
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        let mut p = d.raise("p1", Permission);
        if let EventBody::AttentionRaised(a) = &mut p.body {
            a.summary = format!(
                "Bash: {}",
                (0..14)
                    .map(|i| format!("cargo build -p crate{i:02}"))
                    .collect::<Vec<_>>()
                    .join(" && ")
            );
        }
        control(&mut st, &[d.created("delta"), d.turn_started("build"), p]);
        open_from_needs(&mut st, size);
        let options = st.attention.get("p1").unwrap().options.clone();
        let out = answer_by_taps(&mut st, size, 2);
        assert_eq!(
            responded(&out),
            Some(("p1", revision_of(&st, "p1"), options[1].as_str())),
            "{size:?}: {out:?}"
        );
        // Its whole command, scrolled through.
        let mut st = phone();
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        let mut p = d.raise("p1", Permission);
        if let EventBody::AttentionRaised(a) = &mut p.body {
            a.summary = format!(
                "Bash: {}",
                (0..14)
                    .map(|i| format!("cargo build -p crate{i:02}"))
                    .collect::<Vec<_>>()
                    .join(" && ")
            );
        }
        control(&mut st, &[d.created("delta"), d.turn_started("build"), p]);
        open_from_needs(&mut st, size);
        let seen: String = read_to_end(&mut st, size, 200).into_iter().collect();
        let joined: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(joined.contains("crate13"), "{size:?}: {seen}");

        let mut st = phone();
        raise_long(&mut st, Stalled, "h1", 16, 3);
        open_from_needs(&mut st, size);
        let out = answer_by_taps(&mut st, size, 3);
        assert_eq!(
            responded(&out),
            Some(("h1", revision_of(&st, "h1"), "choice 03")),
            "{size:?}: {out:?}"
        );
    }
}

/// More options than the screen has rows for, under a long text, the keyboard open: the
/// window of rows scrolls (the wheel over them, ↑↓) to the last one, answered by tap and by
/// Enter; a digit answers only an option drawn.
#[test]
fn options_beyond_the_screen_scroll_into_reach() {
    let size = KEYBOARD;
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 12, 14);
    open_from_needs(&mut st, size);
    let out = answer_by_taps(&mut st, size, 14);
    assert_eq!(
        responded(&out),
        Some(("q1", revision_of(&st, "q1"), "choice 14"))
    );
    // By keys: the first ↓ highlights option 1 (the row it opened on, Talk, is out of the
    // window); 9 is not drawn, so its digit waits; ↓ to it, then Enter.
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 12, 14);
    open_from_needs(&mut st, size);
    st.handle_key(key(KeyCode::Down));
    frame(&mut st, size);
    assert_eq!(
        st.view.action.as_ref().unwrap().sel,
        Some(ActionKind::Answer(1))
    );
    let drawn = st.view.panes.popup_rows.clone().unwrap().1;
    assert!(drawn.contains(&ActionKind::Answer(1)), "{drawn:?}");
    assert!(!drawn.contains(&ActionKind::Answer(9)), "{drawn:?}");
    settle(&mut st);
    assert!(
        st.handle_key(key(KeyCode::Char('9'))).is_empty(),
        "9 is not on screen"
    );
    for _ in 1..9 {
        st.handle_key(key(KeyCode::Down));
        frame(&mut st, size);
    }
    let buf = frame(&mut st, size);
    targets_agree(&st, &buf, "option 9 highlighted");
    settle(&mut st);
    let out = st.handle_key(key(KeyCode::Enter));
    assert_eq!(
        responded(&out),
        Some(("q1", revision_of(&st, "q1"), "choice 09"))
    );
}

/// What guarded answers before still does: a digit pressed as the popup first shows the item
/// (within CARD_SETTLE) answers nothing, nor does a first tap on an option (it picks it); once
/// it has been on screen a moment, a digit answers from the first rows, unscrolled, and PgDn
/// and Ctrl-↓ scroll the text without closing the popup.
#[test]
fn an_option_answers_from_the_first_rows_once_the_item_settles() {
    let size = UPRIGHT;
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 40, 3);
    open_from_needs(&mut st, size);
    assert!(
        st.handle_key(key(KeyCode::Char('1'))).is_empty(),
        "just shown"
    );
    settle(&mut st);
    let row = Target::Row(ActionKind::Answer(1));
    let out = tap(&mut st, size, &row).expect("option 1 under the first rows");
    assert!(out.is_empty(), "a first tap picks it: {out:?}");
    settle(&mut st);
    assert_eq!(
        responded(&st.handle_key(key(KeyCode::Char('1')))).map(|r| r.2),
        Some("choice 01")
    );
    assert_eq!(st.view.sheet_view.scroll, 0, "answered unscrolled");
    // Scrolling is there by keys too, the popup staying.
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 40, 3);
    open_from_needs(&mut st, size);
    st.handle_key(key(KeyCode::PageDown));
    frame(&mut st, size);
    st.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
    frame(&mut st, size);
    assert!(st.view.action.is_some());
    assert!(text_at(&st).0 > 0);
    st.handle_key(key(KeyCode::PageUp));
    st.handle_key(key(KeyCode::PageUp));
    frame(&mut st, size);
    assert_eq!(text_at(&st).0, 0);
}

/// A swipe is no tap: pressed on an option, the rows moved under the finger by the wheel,
/// then released, runs nothing (another row is under it); a drag over the options copies text
/// and answers nothing; pressed on one option and released on the next runs neither; a tap on
/// the text's cue pages it, answering nothing.
#[test]
fn swipes_and_drags_never_answer_a_neighbouring_option() {
    let size = KEYBOARD;
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 20, 12);
    open_from_needs(&mut st, size);
    settle(&mut st);
    let left = MouseButton::Left;
    // Picked by a first tap: a second would run it.
    let one = Target::Row(ActionKind::Answer(1));
    assert!(tap(&mut st, size, &one).unwrap().is_empty());
    settle(&mut st);
    let r = rects(&st, &one)[0];
    let (x, y) = (r.x + 2, r.y);
    let screen_now = frame(&mut st, size);
    st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen_now);
    for _ in 0..3 {
        let s = frame(&mut st, size);
        st.handle_mouse(mouse(MouseEventKind::ScrollDown, x, y), &s);
    }
    let s = frame(&mut st, size);
    let out = st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &s);
    assert!(out.is_empty(), "{out:?}");
    // A drag over the rows.
    frame(&mut st, size);
    let p = popup(&st);
    let s = frame(&mut st, size);
    st.handle_mouse(
        mouse(MouseEventKind::Down(left), p.x + 1, p.bottom() - 1),
        &s,
    );
    let s = frame(&mut st, size);
    st.handle_mouse(mouse(MouseEventKind::Drag(left), p.x + 5, p.y), &s);
    let s = frame(&mut st, size);
    let out = st.handle_mouse(mouse(MouseEventKind::Up(left), p.x + 5, p.y), &s);
    assert!(
        out.iter().all(|a| matches!(a, Action::Copy { .. })),
        "a drag copies the text it went over, no more: {out:?}"
    );
    // Pressed on one option, released on another (upright: two of them drawn).
    let size = UPRIGHT;
    for _ in 0..12 {
        frame(&mut st, size);
        let n = rects(&st, &Target::Row(ActionKind::Answer(1))).len()
            + rects(&st, &Target::Row(ActionKind::Answer(2))).len();
        if n == 2 {
            break;
        }
        st.handle_key(key(KeyCode::Up));
    }
    let (a, b) = (
        rects(&st, &Target::Row(ActionKind::Answer(1)))[0],
        rects(&st, &Target::Row(ActionKind::Answer(2)))[0],
    );
    settle(&mut st);
    let s = frame(&mut st, size);
    st.handle_mouse(mouse(MouseEventKind::Down(left), a.x + 2, a.y), &s);
    let s = frame(&mut st, size);
    let out = st.handle_mouse(mouse(MouseEventKind::Up(left), b.x + 2, b.y), &s);
    assert!(
        out.iter().all(|a| matches!(a, Action::Copy { .. })),
        "{out:?}"
    );
    assert!(st.view.action.is_some(), "the popup stays");
    // The text's cue: a tap pages it, the popup staying.
    let more = Target::More(Scrolled::Sheet);
    let before = text_at(&st).0;
    let out = tap(&mut st, size, &more).expect("the text's cue");
    assert!(out.is_empty() && st.view.action.is_some(), "{out:?}");
    assert_ne!(text_at(&st).0, before, "paged");
}

/// The keyboard opening and closing (the height changing) keeps the text's place and the
/// controls on their text, and disarms a picked option (its rows moved); turned on its side
/// (another width, the text wrapped again) it starts at its top. Answered after each.
#[test]
fn a_resize_keeps_the_place_and_the_targets_on_their_text() {
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 30, 4);
    open_from_needs(&mut st, UPRIGHT);
    for _ in 0..6 {
        let p = popup(&st);
        wheel(&mut st, UPRIGHT, p.x + 1, p.y, true);
    }
    let top = |st: &mut AppState, size| {
        let buf = frame(st, size);
        let p = popup(st);
        (p.left()..p.right())
            .map(|x| buf[(x, p.y)].symbol())
            .collect::<String>()
    };
    let was = top(&mut st, UPRIGHT);
    assert!(!was.contains("QUESTION"), "scrolled: {was}");
    settle(&mut st);
    assert!(tap(&mut st, UPRIGHT, &Target::Row(ActionKind::Answer(1)))
        .unwrap()
        .is_empty());
    assert!(st.view.action.as_ref().unwrap().armed.is_some());
    // The keyboard opens: the same row at the top, the pick gone.
    assert_eq!(top(&mut st, KEYBOARD), was);
    assert!(st.view.action.as_ref().unwrap().armed.is_none());
    let buf = frame(&mut st, KEYBOARD);
    targets_agree(&st, &buf, "keyboard");
    let out = answer_by_taps(&mut st, KEYBOARD, 2);
    assert_eq!(responded(&out).map(|r| r.2), Some("choice 02"));
    // Scrolled upright, then turned (wider): from its top, answered there.
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 30, 4);
    open_from_needs(&mut st, UPRIGHT);
    for _ in 0..6 {
        let p = popup(&st);
        wheel(&mut st, UPRIGHT, p.x + 1, p.y, true);
    }
    let wide = (50, 20);
    let buf = frame(&mut st, wide);
    targets_agree(&st, &buf, "wide");
    assert_eq!(text_at(&st).0, 0, "wrapped again, from its top");
    settle(&mut st);
    assert_eq!(
        responded(&st.handle_key(key(KeyCode::Char('2')))).map(|r| r.2),
        Some("choice 02")
    );
}

/// The item raised again while its popup is open (new text, new revision): the text shows
/// from its top, an option picked on it before runs nothing, and a digit pressed as it changes
/// answers nothing; then it is answered under its new revision, from its first rows.
/// Resolved while open, nothing of it answers.
#[test]
fn an_item_replaced_or_resolved_while_open_is_never_answered_as_it_was() {
    let size = KEYBOARD;
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 30, 4);
    open_from_needs(&mut st, size);
    for _ in 0..4 {
        let p = popup(&st);
        wheel(&mut st, size, p.x + 1, p.y, true);
    }
    settle(&mut st);
    let one = Target::Row(ActionKind::Answer(1));
    assert!(tap(&mut st, size, &one).unwrap().is_empty(), "picked");
    let old = revision_of(&st, "q1");
    let mut d = ThreadScript::new("t-delta");
    d.at(200);
    let mut again = d.raise("q1", Question);
    if let EventBody::AttentionRaised(p) = &mut again.body {
        p.summary = (0..30)
            .map(|i| format!("new line {i:02} of the question asked again"))
            .collect::<Vec<_>>()
            .join("\n");
        p.options = vec!["choice 01".into(), "choice 02".into()];
    }
    control(&mut st, &[again]);
    assert_ne!(revision_of(&st, "q1"), old);
    let shown = screen(&mut st, size);
    assert!(shown.contains("QUESTION"), "from its top: {shown}");
    // Tapped (or a digit pressed) as it changed: nothing runs.
    let out = tap(&mut st, size, &one).unwrap_or_default();
    assert!(responded(&out).is_none(), "{out:?}");
    assert!(responded(&st.handle_key(key(KeyCode::Char('1')))).is_none());
    let out = answer_by_taps(&mut st, size, 1);
    assert_eq!(
        responded(&out),
        Some(("q1", revision_of(&st, "q1"), "choice 01"))
    );
    // Resolved while open.
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 30, 4);
    open_from_needs(&mut st, size);
    settle(&mut st);
    let mut d = ThreadScript::new("t-delta");
    d.at(200);
    control(&mut st, &[d.resolve("q1")]);
    frame(&mut st, size);
    settle(&mut st);
    for n in 1..=4 {
        let out = tap(&mut st, size, &Target::Row(ActionKind::Answer(n))).unwrap_or_default();
        assert!(responded(&out).is_none(), "{out:?}");
    }
    assert!(responded(&st.handle_key(key(KeyCode::Char('1')))).is_none());
}

/// The orchestrator's confirm card listing many actions, in the conversation on a phone: a
/// tap on it opens its popup, and Confirm is tapped from its first rows; its whole list is
/// there to scroll through.
#[test]
fn a_long_confirm_card_is_confirmed_from_the_conversation() {
    for size in [UPRIGHT, KEYBOARD] {
        let mut st = worker_orch_and_long_card(24);
        st.view.compact_width = twr_core::config::TuiConfig::default().compact_width;
        frame(&mut st, size);
        assert_eq!(st.view.compact_pane, CompactPane::Chat);
        for _ in 0..2 {
            if st.view.action.is_some() {
                break;
            }
            let at = st
                .view
                .panes
                .get(&PaneId::Thread("t-orch".into()))
                .map(|p| p.area)
                .unwrap_or_else(|| panic!("the confirm card: {}", screen(&mut st, size)));
            tap_at(&mut st, size, at.x + 2, at.y);
        }
        assert!(st.view.action.is_some(), "{}", screen(&mut st, size));
        let out = answer_by_taps(&mut st, size, 1);
        assert_eq!(
            responded(&out),
            Some(("cc1", revision_of(&st, "cc1"), "Confirm")),
            "{size:?}"
        );
    }
    let mut st = worker_orch_and_long_card(24);
    st.view.compact_width = twr_core::config::TuiConfig::default().compact_width;
    frame(&mut st, KEYBOARD);
    st.handle_key(key(KeyCode::Enter));
    assert!(st.view.action.is_some());
    let seen = read_to_end(&mut st, KEYBOARD, 300)
        .into_iter()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(seen.contains("step21"), "{seen}");
}

/// A question waiting for `y` whose prompt is taller than its rows with the keyboard open:
/// never cut, it scrolls (a tap on its `↓ more` row, the wheel over it) to every clause, and
/// `[ yes ]` and `y` answer it whatever it shows.
#[test]
fn a_long_yes_no_prompt_scrolls_and_answers_wherever_it_is() {
    let size = KEYBOARD;
    let ask = |st: &mut AppState| {
        st.view.confirm = Some(crate::app::Confirm {
            action: Action::Stop {
                thread: "t-beta".into(),
            },
            prompt: format!(
                "switch the orchestrator to codex? {} y to confirm, any other key cancels",
                (0..12)
                    .map(|i| format!("clause {i:02} says more."))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            pending: "stopping".into(),
            alt: None,
            offer: false,
        });
    };
    let mut st = phone();
    ask(&mut st);
    let buf = frame(&mut st, size);
    targets_agree(&st, &buf, "prompt");
    assert!(st.view.panes.ask.as_ref().is_some_and(|w| w.max_scroll > 0));
    assert!(lines(&buf).iter().any(|l| l.contains("more lines")));
    // Its rows in the order they came into view.
    let mut rows: Vec<String> = Vec::new();
    for k in 0..40 {
        let buf = frame(&mut st, size);
        targets_agree(&st, &buf, "prompt");
        let w = st.view.panes.ask.clone().expect("the prompt scrolls");
        for y in w.area.top()..w.area.bottom() {
            let row: String = (w.area.left()..w.area.right())
                .map(|x| buf[(x, y)].symbol())
                .collect();
            if !rows.contains(&row) {
                rows.push(row);
            }
        }
        if w.end >= w.rows {
            break;
        }
        if k % 2 == 0 {
            let out = tap(&mut st, size, &Target::More(Scrolled::Ask)).expect("its ↓ more");
            assert!(out.is_empty());
        } else {
            wheel(&mut st, size, w.area.x + 1, w.area.y, true);
        }
        assert!(st.view.confirm.is_some(), "paging it cancels nothing");
    }
    let seen = rows.concat();
    let flat: String = seen.chars().filter(|c| !c.is_whitespace()).collect();
    for i in 0..12 {
        assert!(
            flat.contains(&format!("clause{i:02}")),
            "clause {i}: {seen}"
        );
    }
    assert!(flat.contains("anyotherkeycancels"), "{seen}");
    // From its first rows: `yes` (once on screen a moment, as before) and `y`.
    let mut st = phone();
    ask(&mut st);
    frame(&mut st, size);
    settle(&mut st);
    assert!(matches!(
        tap(&mut st, size, &Target::Yes)
            .expect("yes from the first rows")
            .as_slice(),
        [Action::Stop { .. }]
    ));
    let mut st = phone();
    ask(&mut st);
    frame(&mut st, size);
    assert!(matches!(
        st.handle_key(key(KeyCode::Char('y'))).as_slice(),
        [Action::Stop { .. }]
    ));
}

/// The shortest phones: on its side (80 x 9) a cut prompt keeps its cue row above `yes` and
/// `no`, a row of the prompt above it.
#[test]
fn a_cut_prompt_on_its_side_shows_its_cue() {
    let size = LANDSCAPE;
    let mut st = phone();
    st.view.confirm = Some(crate::app::Confirm {
        action: Action::Stop {
            thread: "t-beta".into(),
        },
        prompt: format!(
            "stop beta? {} y to confirm, any other key cancels",
            "it has a long explanation of what stopping it does. ".repeat(6)
        ),
        pending: "stopping".into(),
        alt: None,
        offer: false,
    });
    for _ in 0..40 {
        let buf = frame(&mut st, size);
        targets_agree(&st, &buf, "landscape prompt");
        assert!(!rects(&st, &Target::Yes).is_empty(), "yes drawn throughout");
        let w = st.view.panes.ask.clone().expect("the prompt's window");
        assert!(w.end > w.start, "a row of the prompt shows");
        if w.end >= w.rows {
            break;
        }
        let shown = lines(&buf).join("\n");
        assert!(shown.contains("more lines"), "{shown}");
        tap(&mut st, size, &Target::More(Scrolled::Ask)).expect("its cue");
    }
}

/// A free-text reply: a long draft in the input box (which grows) leaves a long item's popup
/// its text and options (the popup covers the box), and the draft stays; `talk to` it opens its
/// conversation with what was typed there to send.
#[test]
fn a_long_draft_leaves_the_popup_its_rows_and_talk_takes_the_reply() {
    let size = KEYBOARD;
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 20, 4);
    frame(&mut st, size);
    let draft = (0..8)
        .map(|i| format!("draft line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    st.handle_paste(&draft);
    open_from_needs(&mut st, size);
    let seen = read_to_end(&mut st, size, 200);
    assert!(seen.iter().any(|r| r.contains("line 19:")), "{seen:?}");
    let out = answer_by_taps(&mut st, size, 4);
    assert_eq!(responded(&out).map(|r| r.2), Some("choice 04"));
    assert_eq!(st.view.input.as_str(), draft, "the draft kept");
    // Talk to it: its conversation, the reply typed in the box.
    let mut st = phone();
    raise_long(&mut st, Question, "q1", 20, 4);
    frame(&mut st, size);
    st.handle_paste("my own answer, typed");
    open_from_needs(&mut st, size);
    let talk = Target::Row(ActionKind::Talk);
    for _ in 0..30 {
        frame(&mut st, size);
        if !rects(&st, &talk).is_empty() {
            break;
        }
        st.handle_key(key(KeyCode::Up));
    }
    tap(&mut st, size, &talk).expect("talk to delta");
    if st.view.dive.is_none() {
        tap(&mut st, size, &talk);
    }
    assert_eq!(st.view.dive.as_deref(), Some("t-delta"));
    assert_eq!(st.view.input.as_str(), "my own answer, typed");
}

/// The rows never move under the finger: a tap picking an option (the window at its top, and
/// scrolled down by ↓) leaves every row where it was drawn, so from its own spot each option
/// is picked and only the one tapped twice runs.
#[test]
fn a_tap_never_moves_the_rows_under_the_finger() {
    let answers = |st: &AppState| -> Vec<(Rect, Target)> {
        st.view
            .panes
            .targets
            .iter()
            .filter(|(_, t)| matches!(t, Target::Row(_)))
            .cloned()
            .collect()
    };
    for size in [UPRIGHT, KEYBOARD, LANDSCAPE] {
        for down in [0, 9] {
            let mut st = phone();
            raise_long(&mut st, Question, "q1", 24, 12);
            open_from_needs(&mut st, size);
            for _ in 0..down {
                st.handle_key(key(KeyCode::Down));
                frame(&mut st, size);
            }
            settle(&mut st);
            frame(&mut st, size);
            let drawn = answers(&st);
            assert!(!drawn.is_empty());
            for (r, t) in drawn.clone() {
                settle(&mut st);
                let out = tap_at(&mut st, size, r.x + 2, r.y);
                assert!(responded(&out).is_none(), "{size:?}: {out:?}");
                assert_eq!(
                    answers(&st),
                    drawn,
                    "{size:?}: a tap on {t:?} moved the rows"
                );
                let picked = match &t {
                    Target::Row(k) => Some(*k),
                    _ => None,
                };
                assert_eq!(st.view.action.as_ref().unwrap().sel, picked);
            }
            let (r, t) = drawn.last().cloned().unwrap();
            settle(&mut st);
            let out = tap_at(&mut st, size, r.x + 2, r.y);
            if let Target::Row(ActionKind::Answer(n)) = t {
                assert_eq!(
                    responded(&out).map(|r| r.2.to_string()),
                    Some(format!("choice {n:02}")),
                    "{size:?}"
                );
            }
        }
    }
}

/// A worker's report longer than the screen: all of it scrolls into view in its popup; another
/// report coming for it starts from its top.
#[test]
fn a_long_report_scrolls_and_a_new_one_starts_at_its_top() {
    let size = KEYBOARD;
    let mut st = phone();
    let mut c = ThreadScript::new("t-gamma");
    c.at(300);
    let report = |c: &mut ThreadScript, word: &str| {
        let mut ev = c.report(Some(1), twr_core::event::Grade::Success);
        if let EventBody::ReportReady(r) = &mut ev.body {
            r.summary = (0..20)
                .map(|i| format!("{word} paragraph {i:02} of the report."))
                .collect::<Vec<_>>()
                .join("\n");
        }
        ev
    };
    let first = report(&mut c, "first");
    control(&mut st, &[first]);
    frame(&mut st, size);
    type_line(&mut st, "/t");
    st.set_focus(Some("t-gamma".into()));
    st.handle_key(key(KeyCode::Enter));
    assert_eq!(
        st.view.action.as_ref().map(|p| p.target.clone()),
        Some(crate::app::ActionTarget::Card("t-gamma".into()))
    );
    let seen = read_to_end(&mut st, size, 100);
    for i in 0..20 {
        let line = format!("first paragraph {i:02}");
        assert!(seen.iter().any(|r| r.contains(&line)), "{line}: {seen:?}");
    }
    let second = report(&mut c, "second");
    control(&mut st, &[second]);
    let shown = screen(&mut st, size);
    assert!(
        shown.contains("second paragraph 00"),
        "from its top: {shown}"
    );
}

/// A queued message longer than the popup: Delete runs from its first rows, as on a desktop;
/// all of it is there to scroll through.
#[test]
fn a_long_queued_message_scrolls_and_is_deleted_from_its_first_rows() {
    let size = KEYBOARD;
    let open = || {
        let mut st = with_orch();
        st.view.compact_width = twr_core::config::TuiConfig::default().compact_width;
        assert_eq!(st.queue_due(Instant::now()).as_deref(), Some("t-orch"));
        let prompt = (0..20)
            .map(|i| format!("queued line {i:02} of a long message"))
            .collect::<Vec<_>>()
            .join("\n");
        st.apply_result(
            "thread.queue",
            &serde_json::json!({"thread": "t-orch", "thread_name": "orchestrator", "total": 1,
                    "held": false, "items": [{"ref": 7, "prompt": prompt, "truncated": false,
                    "from": "user", "mine": true}]}),
        );
        frame(&mut st, size);
        st.select_queued(7);
        frame(&mut st, size);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            st.view.action.as_ref().map(|p| p.target.clone()),
            Some(crate::app::ActionTarget::Queued(7))
        );
        frame(&mut st, size);
        st
    };
    let mut st = open();
    let seen = read_to_end(&mut st, size, 100);
    assert!(
        seen.iter().any(|r| r.contains("queued line 19")),
        "{seen:?}"
    );
    let mut st = open();
    assert_eq!(text_at(&st).0, 0);
    for _ in 0..10 {
        if st.view.action.as_ref().unwrap().sel == Some(ActionKind::Delete) {
            break;
        }
        st.handle_key(key(KeyCode::Down));
        frame(&mut st, size);
    }
    assert!(
        !st.handle_key(key(KeyCode::Enter)).is_empty(),
        "deleted unscrolled"
    );
}

/// An option of many rows is listed in full in the text (its row by its first line), so its
/// whole label can be read, and it is answered.
#[test]
fn a_tall_option_is_listed_in_full_in_the_text() {
    let mut st = phone();
    let mut d = ThreadScript::new("t-delta");
    d.at(70);
    let mut q = d.raise("q1", Question);
    if let EventBody::AttentionRaised(p) = &mut q.body {
        p.summary = "Which plan?".into();
        p.options = vec![
            (0..8)
                .map(|i| format!("plan step {i} of the first option"))
                .collect::<Vec<_>>()
                .join("\n"),
            "the second".into(),
        ];
    }
    control(&mut st, &[d.created("delta"), d.turn_started("ask"), q]);
    open_from_needs(&mut st, KEYBOARD);
    let shown = screen(&mut st, KEYBOARD);
    assert!(shown.contains("(in full above)"), "{shown}");
    let seen = read_to_end(&mut st, KEYBOARD, 100);
    assert!(seen.iter().any(|r| r.contains("plan step 7")), "{seen:?}");
    let out = answer_by_taps(&mut st, UPRIGHT, 1);
    assert!(
        responded(&out).is_some_and(|r| r.2.starts_with("plan step 0")),
        "{out:?}"
    );
}

/// The shortest phone screen (80 x 8 on its side): an option of three rows is listed in full
/// in the text and by its first line as a row of two, so every option can be answered.
#[test]
fn every_option_is_reachable_on_the_shortest_screen() {
    let size = (80, 8);
    let mut st = phone();
    let mut d = ThreadScript::new("t-delta");
    d.at(70);
    let mut q = d.raise("q1", Question);
    if let EventBody::AttentionRaised(p) = &mut q.body {
        p.summary = "Which?".into();
        p.options = vec!["first\nsecond row\nthird row".into(), "plain".into()];
    }
    control(&mut st, &[d.created("delta"), d.turn_started("ask"), q]);
    open_from_needs(&mut st, size);
    let buf = frame(&mut st, size);
    targets_agree(&st, &buf, "80x8");
    let out = answer_by_taps(&mut st, size, 1);
    assert_eq!(
        responded(&out).map(|r| r.2),
        Some("first\nsecond row\nthird row"),
        "{}",
        screen(&mut st, size)
    );
}
