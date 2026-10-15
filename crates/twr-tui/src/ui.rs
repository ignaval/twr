//! Pure rendering of the stack, grid and decisions views (the stack view: the main pane with
//! the orchestrator conversation or the thread dived into, and the worker cards): `render(frame, &AppState)` draws everything from
//! state and does no IO, so it is tested with ratatui's `TestBackend`. It returns the text
//! panes it drew ([`Panes`]), which the mouse selection works on.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;
use serde_json::Value;
use twr_core::event::GitSnapshot;
use twr_core::event::{AttentionReason, TurnFrom, TurnResult, HUB_THREAD};
use twr_core::reduce::{AttentionItem, RequestContext, ThreadStatus, ThreadView};

use crate::app::{
    grid_columns, provider_label, reason_label, site_label, summary_str, AppState, DecisionRow,
    Entry, EscDoes, NoticeLevel, Screen, DECISIONS_CAP, PROVIDERS, TWR_TOOL,
};
use crate::edit::{cluster_width, clusters};
use crate::icon;
use crate::select::{
    Anchor, DrawnItem, Join, ListLayout, ListWindow, Pane, PaneId, Panes, RequestCard, Target,
};

/// Height of a card with an attention item (and of the card row), borders included.
pub const CARD_HEIGHT: u16 = 11;
/// Height of a card without an attention item in the side column, borders included: its
/// status, activity and git rows and two of its report.
pub const COMPACT_CARD_HEIGHT: u16 = 7;
/// Narrowest a card may get before cards start scrolling horizontally.
pub const CARD_MIN_WIDTH: u16 = 30;
/// Narrowest terminal whose cards go in a side column next to the main pane; narrower ones
/// get a row of cards above it.
pub const SIDE_MIN_WIDTH: u16 = 100;
/// Height of a grid tile, borders included.
pub const TILE_HEIGHT: u16 = 9;
/// Rows of text the input box grows to at most (fewer on a short screen, [`input_cap`]).
pub const INPUT_MAX_ROWS: usize = 6;
/// Rows the popup above the input line (`/` commands, `@` mentions) lists at most.
pub const POPUP_ROWS: usize = 8;
/// Shown when the hub has no worker threads.
pub const EMPTY_STATE: &str = "no worker threads — /new <project>, or ask the orchestrator";

/// Width of the side column of cards at terminal width `width`.
pub fn side_width(width: u16) -> u16 {
    (width * 3 / 10).clamp(34, 48)
}

/// Draws the current view (stack, grid or decisions), the orchestrator picker, the help or
/// the trash overlay, the action popup or the `/` popup over it when open, and the mouse
/// selection. Returns the text panes drawn.
pub fn render(frame: &mut Frame, state: &AppState) -> Panes {
    state.wraps.next_frame();
    let mut panes = Panes::default();
    let area = frame.area();
    let body = if state.tabs_shown() && area.height > 1 {
        let [strip, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        render_tabs(frame, strip, state, &mut panes);
        body
    } else {
        area
    };
    match state.view.screen {
        Screen::Decisions => render_decisions_view(frame, body, state, &mut panes),
        // A phone: one screen at a time.
        _ if state.compact() => render_compact(frame, body, state, &mut panes),
        Screen::Stack => render_stack(frame, body, state, &mut panes, false),
        Screen::Grid => render_grid_view(frame, body, state, &mut panes),
    }
    // What an overlay hides is not on screen: no option key may answer it, no control under
    // it is tapped. Each covers what is under it itself, then records its own item and
    // controls. Overlays stay below the tab strip, which keeps its tabs on screen and
    // clickable.
    if state.picker_open() {
        render_picker(frame, body, state, &mut panes);
    } else if state.view.help {
        render_help(frame, body, state.view.help_scroll, &mut panes);
    } else if let Some(trash) = &state.view.trash {
        render_trash(frame, body, trash, state.view.trash_scroll, &mut panes);
    } else if render_action_popup(frame, body, state, &mut panes).is_some() {
    } else {
        render_popup(frame, body, state, &mut panes);
    }
    if let Some(sel) = &state.view.selection {
        sel.highlight(frame.buffer_mut());
    }
    // The clock ticks only for a moving cell still on screen once the overlays are drawn
    // (never one off the buffer).
    let screen = frame.area();
    panes.moving.retain(|c| on(screen, *c));
    panes.animated = !panes.moving.is_empty();
    panes
}

/// The workspaces' tab strip ([`crate::workspace`]): each tab's name with its badges (`⚠n`
/// items waiting on the user, `●n` threads running, `○` a stopped hub, `…` one that did not
/// answer), the current one highlighted. A click on a tab switches to it
/// ([`PaneId::Tab`]). When they don't all fit, the tabs around the current one show, with
/// `‹` / `›` for those left out.
fn render_tabs(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    use crate::workspace::TabHub;
    let width = usize::from(area.width);
    let labels: Vec<(String, String, Option<String>, bool)> = state
        .tabs
        .iter()
        .map(|t| {
            let current = state.workspace.as_deref() == Some(t.name.as_str());
            let hub = state.tab_hub(t);
            let waiting = hub.waiting();
            let (badges, waiting) = match hub {
                TabHub::Unknown => (String::new(), None),
                TabHub::Stopped => (" ○".to_string(), None),
                TabHub::Busy => (" …".to_string(), None),
                TabHub::Running { running, .. } => (
                    if running > 0 {
                        format!(" ●{running}")
                    } else {
                        String::new()
                    },
                    (waiting > 0).then(|| format!(" ⚠{waiting}")),
                ),
            };
            (t.name.clone(), badges, waiting, current)
        })
        .collect();
    let cells = |(name, badges, waiting, _): &(String, String, Option<String>, bool)| {
        crate::edit::width(name)
            + crate::edit::width(badges)
            + waiting.as_deref().map_or(0, crate::edit::width)
            + 2
    };
    // The tabs around the current one, as many as fit with the separators and markers.
    let n = labels.len();
    let at = labels.iter().position(|l| l.3).unwrap_or(0);
    let (mut first, mut last) = (at, at);
    let used = |first: usize, last: usize| {
        let tabs: usize = labels[first..=last].iter().map(cells).sum();
        let seps = last - first;
        let marks = usize::from(first > 0) * 2 + usize::from(last + 1 < n) * 2;
        tabs + seps + marks
    };
    loop {
        let grew_right = last + 1 < n && used(first, last + 1) <= width;
        if grew_right {
            last += 1;
        }
        let grew_left = first > 0 && used(first - 1, last) <= width;
        if grew_left {
            first -= 1;
        }
        if !grew_right && !grew_left {
            break;
        }
    }
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    let mut x = area.x;
    let push = |spans: &mut Vec<Span<'static>>, text: String, style: Style| -> u16 {
        let w = crate::edit::width(&text) as u16;
        spans.push(Span::styled(text, style));
        w
    };
    if first > 0 {
        x += push(&mut spans, "‹ ".into(), dim);
    }
    for (i, label) in labels.iter().enumerate().take(last + 1).skip(first) {
        if i > first {
            x += push(&mut spans, "│".into(), dim);
        }
        let (name, badges, waiting, current) = label;
        let base = if *current {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        let start = x;
        x += push(&mut spans, format!(" {name}"), base);
        if let Some(w) = waiting {
            x += push(&mut spans, w.clone(), base.fg(Color::Yellow));
        }
        x += push(&mut spans, format!("{badges} "), base);
        let tab = Rect::new(start, area.y, x.saturating_sub(start), 1).intersection(area);
        panes.add(PaneId::Tab(name.clone()), tab);
    }
    if last + 1 < n {
        push(&mut spans, " ›".into(), dim);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Whether `cell` lies wholly inside `area` (and is no empty rect).
fn on(area: Rect, cell: Rect) -> bool {
    !cell.is_empty() && area.intersection(cell) == cell
}

/// The `✕` an overlay's top border ends in: the whole border row is its control
/// ([`close_row`]).
fn close_title() -> Line<'static> {
    Line::from(" ✕ ").right_aligned()
}

/// Records the top border row of an overlay drawn over `rect` as control `target` (its `✕`,
/// [`close_title`]): a row wide, for a finger.
fn close_row(panes: &mut Panes, rect: Rect, target: Target) {
    panes.target(
        Rect::new(rect.x, rect.y, rect.width, rect.height.min(1)),
        target,
    );
}

/// Where `needle` was drawn in `area` of `buf`, on one row: its cells. A wide character
/// counts once, at its first cell.
fn find_text(buf: &Buffer, area: Rect, needle: &str) -> Option<Rect> {
    let area = area.intersection(buf.area);
    for y in area.y..area.bottom() {
        let mut text = String::new();
        let mut xs = Vec::new();
        for x in area.x..area.right() {
            let symbol = buf[(x, y)].symbol();
            for _ in symbol.chars() {
                xs.push(x);
            }
            text.push_str(symbol);
        }
        if let Some(at) = text.find(needle) {
            let first = text[..at].chars().count();
            let last = first + needle.chars().count() - 1;
            let (x0, x1) = (xs[first], xs[last]);
            return Some(Rect::new(x0, y, x1 - x0 + 1, 1));
        }
    }
    None
}

/// Draws `state` on an off-screen terminal `width` x `height` the way the IO loop draws a
/// frame: the width and the panes drawn are kept, and what the frame showed is noted at `now`
/// ([`AppState::note_drawn`]). Returns the screen. For tests and tools that need no terminal.
pub fn draw_offscreen(
    state: &mut AppState,
    width: u16,
    height: u16,
    now: std::time::Instant,
) -> ratatui::buffer::Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    (state.view.width, state.view.height) = (width, height);
    let mut panes = Panes::default();
    let buffer = Terminal::new(TestBackend::new(width, height))
        .and_then(|mut term| {
            term.draw(|f| panes = render(f, state))?;
            Ok(term.backend().buffer().clone())
        })
        .unwrap_or_default();
    state.note_panes(panes);
    if let Some(sel) = state.view.selection.as_mut() {
        sel.refresh(&state.view.panes);
    }
    state.note_drawn(now);
    buffer
}

/// Keys listed by the help overlay: those that work whatever is typed, those about the
/// selection, and the views.
pub const HELP_KEYS: &[(&str, &str)] = &[
    (
        "Enter",
        "send what you typed (to the orchestrator, in a dive to that thread); on an empty line, \
         about the selection: a long block expands or collapses, a card or a request opens its \
         actions",
    ),
    (
        "actions",
        "1-9 answer the item shown; ↑↓ pick a row, Enter runs it (talk to it, dismiss, pin, \
         stop, park or resume, attach); letters filter the rows; Esc closes. Enter, Enter talks \
         to a worker directly",
    ),
    (
        "Tab, Shift-Tab",
        "select the next, the previous card (Tab completes a / command or a popup's row first)",
    ),
    (
        "Shift-↑↓",
        "select the previous, the next card; with the conversation selected, its previous, its \
         next long block or request",
    ),
    (
        "Shift-←, Shift-→",
        "select the conversation, the cards again; in the grid the Shift-arrows move between \
         tiles",
    ),
    (
        "Esc",
        "close a popup or an overlay; else release the selection (a block, a request, the \
         conversation, then the card); else, in a dive, back to the orchestrator. It never \
         clears what you typed",
    ),
    (
        "Esc Esc",
        "with nothing typed, take back the message you just sent, while nothing was done \
         with it yet (the first Esc says so in the box's title; /unsend does it too)",
    ),
    (
        "Ctrl-↑↓, Alt-↑↓",
        "scroll the main pane a line (Alt where the system takes Ctrl-↑↓, as on macOS)",
    ),
    (
        "PgUp, PgDn, wheel",
        "scroll the main pane; scrolled up, it stays on what you read as text streams in, and \
         its bottom border says how much is below. The wheel over the cards scrolls them",
    ),
    (
        "Ctrl-Home, Ctrl-End",
        "the main pane's top; its bottom, which follows new text again (as does scrolling down \
         to it). Home and End too, on an empty line. Another conversation or dive opens at its \
         bottom",
    ),
    (
        "Ctrl-G",
        "the grid of threads, or back to the stack (/grid)",
    ),
    ("Ctrl-B", "show or hide the thread sidebar (/sidebar)"),
    (
        "Ctrl-F",
        "Focus mode: working threads and anything needing you (/focus)",
    ),
    (
        "Ctrl-O",
        "show or hide tool calls (the orchestrator's, or in a dive the thread's) and the hub's \
         updates",
    ),
    ("Ctrl-L", "repaint the whole screen"),
    (
        "?",
        "this help, typed on an empty line (after text it types)",
    ),
    ("Ctrl-C", "quit (/quit)"),
];

/// Keys of the input line, listed by the help overlay.
pub const HELP_EDIT_KEYS: &[(&str, &str)] = &[
    ("←→", "move the cursor"),
    (
        "↑↓",
        "a row up or down in wrapped text; on the first row, Up recalls the lines you sent \
         before (Down goes forward again, past the newest back to what you were typing)",
    ),
    (
        "Home, End",
        "to the start, to the end of the row; on an empty line, the main pane's top and \
         bottom",
    ),
    (
        "Ctrl-A, Ctrl-E",
        "to the start, to the end of the whole text",
    ),
    ("Ctrl-←→, Alt-←→, Alt-B/F", "move by word"),
    (
        "Backspace, Del, Ctrl-W, Ctrl-K",
        "delete before, at the cursor, the word before, to the end of the line",
    ),
    ("Ctrl-U", "clear the text"),
    ("Enter", "send the text, all its lines"),
    (
        "Shift-Enter, Alt-Enter, Ctrl-J",
        "a new line (where the terminal sends Shift-Enter as Enter, Alt-Enter or Ctrl-J)",
    ),
    (
        "↑↓, Ctrl-P/N, Tab, Esc",
        "in the / or @ popup: pick, complete, close",
    ),
    (
        "@",
        "at the start of a word: pick a worker's name (Tab or Enter puts it in)",
    ),
];

/// The mouse and touch (a tap on a phone is a click), listed by the help overlay.
pub const HELP_MOUSE: &[(&str, &str)] = &[
    (
        "drag",
        "select and copy text in one pane (Shift-drag: the terminal's own selection)",
    ),
    (
        "click, tap",
        "a card, tile, list row or request selects it, and on the one selected opens its \
         actions (a quick double click on a card or tile dives in); a folded line expands, a \
         long block's footer expands or collapses it; the input box puts the cursor there",
    ),
    (
        "actions",
        "a tap picks a row, a tap on the picked row runs it; an answer, dismiss or stop only \
         once a tap picked it with the item on screen long enough to read. ✕ or a tap \
         outside closes it",
    ),
    (
        "controls",
        "Chat · Needs you · Threads switch screens; ‹ back leaves a dive, ‹ stack the grid; \
         the ↓ below border follows the bottom; ✕ closes an overlay, a popup or a detail \
         view; ↶ take back, tapped twice, takes back the message you just sent; [ yes ] and \
         [ no ] answer a question; a tap on a notice clears it, on a note does what its key \
         says",
    ),
    (
        "wheel",
        "scroll what is under the pointer: the conversation, a detail view, the cards, a list, \
         the help or the trash; over a popup, the picker or the decision table, move its \
         highlight",
    ),
];

/// The compact layout and mosh, listed by the help overlay.
pub const HELP_PHONE: &[(&str, &str)] = &[
    (
        "compact layout",
        "below [tui] compact_width columns (60), or /compact on: one screen at a time, Chat, \
         Needs you and Threads, switched with /c, /n and /t or a tap on their names; ↑↓ on an \
         empty line pick a row, Enter opens it, its digits answer it; a tap picks a row, a tap \
         on the picked one opens it",
    ),
    (
        "Enter",
        "on an empty line in the conversation: the oldest item waiting on you",
    ),
    (
        "mosh",
        "on a slow link, mosh --predict-overwrite keeps its typing echo from shifting what is \
         on screen",
    ),
];

/// Below this many columns the help overlay puts each command or key on a row of its own,
/// with what it does indented under it ([`help_lines`]).
pub const HELP_STACKED_BELOW: u16 = 70;
/// The help overlay's widest, borders included.
const HELP_MAX_WIDTH: u16 = 110;
/// The narrowest what a command or key does gets beside it in the help's table; an entry
/// that would leave it less has a row of its own.
const HELP_TEXT_MIN: usize = 30;

/// The help overlay's rows, `width` cells wide: the typed commands, then the keys, each a
/// table of a command or key and what it does. Overlays [`HELP_STACKED_BELOW`] columns wide
/// and wider give a table's first column the width of its longest entry (unless that would
/// leave what it does fewer than [`HELP_TEXT_MIN`] columns: a longer entry then has a row of
/// its own), and what it does wraps beside it; narrower, each command or key has a row of its
/// own, with what it does wrapped under it, indented.
pub fn help_lines(width: usize) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut commands: Vec<(&str, &str)> = crate::slash::COMMANDS
        .iter()
        .map(|(_, usage, what, _)| (*usage, *what))
        .collect();
    commands.push(("//text", "send a line that starts with /"));
    commands.push((
        "anything else",
        "send it to the orchestrator (in a dive: to that thread)",
    ));
    let stacked = width + 2 < usize::from(HELP_STACKED_BELOW);
    let mut out = Vec::new();
    for (i, (title, table)) in [
        ("Typed commands", commands),
        ("Keys", HELP_KEYS.to_vec()),
        ("Keys (typing)", HELP_EDIT_KEYS.to_vec()),
        ("Mouse and touch", HELP_MOUSE.to_vec()),
        ("On a phone", HELP_PHONE.to_vec()),
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            out.push(Line::from(""));
        }
        out.extend(
            wrap(title, width.max(1))
                .into_iter()
                .map(|row| Line::from(Span::styled(row, bold))),
        );
        // Two spaces, the entry, two spaces.
        let longest = table
            .iter()
            .map(|(k, _)| crate::edit::width(k))
            .max()
            .unwrap_or(0);
        let column = if stacked {
            4.min(width / 2)
        } else {
            (longest + 4).min(width.saturating_sub(HELP_TEXT_MIN))
        };
        for (k, what) in table {
            out.extend(help_entry(k, what, width, column, stacked));
        }
    }
    out
}

/// The help's rows for entry `key` doing `what`, `width` cells wide: `key` (cyan, after two
/// spaces) with `what` wrapped beside it from cell `column` on, or, `stacked` or too long for
/// the column, `key` on rows of its own and `what` wrapped under it from cell `column`.
fn help_entry(
    key: &str,
    what: &str,
    width: usize,
    column: usize,
    stacked: bool,
) -> Vec<Line<'static>> {
    let cyan = Style::default().fg(Color::Cyan);
    let text = wrap(what, width.saturating_sub(column).max(1));
    let indent = " ".repeat(column);
    let key_width = crate::edit::width(key);
    let mut out = Vec::new();
    let mut text = text.into_iter();
    if stacked || key_width + 4 > column {
        // Two spaces, fewer on the narrowest screens: a cell is left for the text.
        let lead = 2.min(width / 2);
        for row in wrap(key, width.saturating_sub(lead).max(1)) {
            out.push(Line::from(Span::styled(
                format!("{}{row}", " ".repeat(lead)),
                cyan,
            )));
        }
    } else {
        let pad = " ".repeat(column - 2 - key_width);
        out.push(Line::from(vec![
            Span::styled(format!("  {key}{pad}"), cyan),
            Span::raw(text.next().unwrap_or_default()),
        ]));
    }
    out.extend(text.map(|row| Line::from(format!("{indent}{row}"))));
    out
}

/// A `width` x `height` rect centered in `area` (clipped to it).
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (area.width.min(width), area.height.min(height));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// The help overlay, centered over whatever is on screen, `scroll` rows down. Its rows wrap
/// to its width ([`help_lines`]); taller than the screen, it scrolls (↑↓, PgUp/PgDn,
/// Home/End, the wheel), and its bottom border says `↓ more` while there is more below.
fn render_help(frame: &mut Frame, area: Rect, scroll: u16, panes: &mut Panes) {
    let width = area.width.min(HELP_MAX_WIDTH);
    let lines = help_lines(usize::from(width.saturating_sub(2)).max(1));
    let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let rect = centered(area, width, rows.saturating_add(2));
    let fit = rect.height.saturating_sub(2);
    panes.help_max = rows.saturating_sub(fit);
    let scroll = scroll.min(panes.help_max);
    let hints: &[&str] = if scroll < panes.help_max {
        &[
            " ↓ more · ↑↓ PgUp/PgDn Home/End scroll · Esc closes ",
            " ↓ more · ↑↓ scroll · Esc closes ",
            " ↓ more · Esc closes ",
            " ↓ more ",
        ]
    } else if panes.help_max > 0 {
        &[
            " ↑↓ PgUp/PgDn Home/End scroll · Esc closes ",
            " ↑↓ scroll · Esc closes ",
            " Esc closes ",
        ]
    } else {
        &[" Esc or q closes this help ", " Esc closes "]
    };
    let room = usize::from(width.saturating_sub(2));
    let hint = hints
        .iter()
        .find(|h| crate::edit::width(h) <= room)
        .copied()
        .unwrap_or_default();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" twr help ")
        .title(close_title())
        // On the border: shown whatever the scroll.
        .title_bottom(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    panes.cover(None);
    panes.add(PaneId::Help, block.inner(rect));
    close_row(panes, rect, Target::Close);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), rect);
}

/// The trash overlay's lines for a `trash.list` result: one row per item (short id, thread,
/// project, box, deleted and purge times, size, what it loses, whether it was restored), the
/// boxes that could not answer, the hub's message and the commands.
pub fn trash_lines(r: &Value) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let cyan = Style::default().fg(Color::Cyan);
    let s = |v: &Value| v.as_str().unwrap_or("-").to_string();
    let time = |v: &Value| {
        let t = v.as_str().unwrap_or("-");
        t.get(..16).map_or(t.to_string(), |h| h.replace('T', " "))
    };
    let mut out = vec![Line::from(Span::styled("Trash", bold))];
    let items = r["items"].as_array().cloned().unwrap_or_default();
    for i in &items {
        let mut what = i["losses"].as_str().unwrap_or("").to_string();
        if i["box_offline"].as_bool() == Some(true) {
            what.push_str(" (box offline)");
        }
        if i["incomplete"].as_bool() == Some(true) {
            match i["kept"].as_str() {
                Some(why) => what.push_str(&format!(" (incomplete, kept: {why})")),
                None => what.push_str(" (incomplete)"),
            }
        }
        if let Some(n) = i["restored_as"].as_str() {
            what.push_str(&format!(
                " (restored as {n} on {})",
                time(&i["restored_at"])
            ));
        }
        // Two lines an item: what it is, then when it goes and what it loses.
        out.push(Line::from(vec![
            Span::styled(format!("  {:<9} ", s(&i["short"])), cyan),
            Span::styled(s(&i["thread_name"]), bold),
            Span::raw(format!(
                "  {} on {} · {}",
                s(&i["project"]),
                s(&i["box"]),
                s(&i["size"]),
            )),
        ]));
        let mut when = vec![Span::raw(format!(
            "            deleted {} · purged {}",
            time(&i["deleted_at"]),
            time(&i["purge_at"]),
        ))];
        if !what.trim().is_empty() {
            when.push(Span::styled(format!(" · {}", what.trim()), dim));
        }
        out.push(Line::from(when));
    }
    for u in r["unavailable"].as_array().into_iter().flatten() {
        out.push(Line::from(Span::styled(
            format!(
                "  box {} could not list its trash: {}",
                s(&u["box"]),
                s(&u["error"])
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    out.push(Line::from(""));
    out.push(Line::from(s(&r["message"])));
    out.push(Line::from(Span::styled(
        "/trash restore <id> [as <name>] · /trash empty <id>...|--all · any key closes this",
        dim,
    )));
    out
}

/// The trash overlay, centered over whatever is on screen.
fn render_trash(frame: &mut Frame, area: Rect, trash: &Value, scroll: u16, panes: &mut Panes) {
    // Wrapped to the overlay's width: nothing runs past its border.
    let width = area.width.min(120);
    let inner = usize::from(width.saturating_sub(2)).max(1);
    let lines: Vec<Line<'static>> = trash_lines(trash)
        .into_iter()
        .flat_map(|l| wrap_line(l, inner))
        .collect();
    let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let rect = centered(area, width, rows.saturating_add(2));
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" twr trash ")
        .title(close_title());
    // Taller than the screen: it scrolls, and says so.
    let fit = rect.height.saturating_sub(2);
    panes.trash_max = rows.saturating_sub(fit);
    let scroll = scroll.min(panes.trash_max);
    if rows > fit {
        block = block.title_bottom(
            Line::from(format!(
                " rows {}-{} of {rows} · ↑↓ PgUp/PgDn scroll · any other key closes ",
                scroll + 1,
                (scroll + fit).min(rows)
            ))
            .right_aligned(),
        );
    }
    panes.cover(None);
    panes.add(PaneId::Help, block.inner(rect));
    close_row(panes, rect, Target::Close);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), rect);
}

/// The orchestrator picker's lines: what it is for, one row per runtime (installed or not,
/// the configured default marked, or for a switch the current one), the keys.
pub fn picker_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(p) = &state.view.picker else {
        return Vec::new();
    };
    let dim = Style::default().fg(Color::DarkGray);
    let name_width = p.runtimes.iter().map(|(r, _)| r.len()).max().unwrap_or(0);
    let (title, note) = if p.switch {
        (
            "Switch the orchestrator to",
            "(a new session that starts with a handoff; you confirm it next)",
        )
    } else {
        (
            "Pick the agent that runs your orchestrator",
            "(you can switch any time with /orch)",
        )
    };
    let mut out = vec![
        Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(note, dim)),
        Line::from(""),
    ];
    for (i, (runtime, installed)) in p.runtimes.iter().enumerate() {
        let chosen = i == p.selected;
        let mut spans = vec![
            Span::raw(if chosen { "› " } else { "  " }),
            Span::styled(
                format!("{} ", i + 1),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{runtime:<name_width$}  "),
                if chosen {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default()
                },
            ),
        ];
        match installed {
            Some(true) => spans.push(Span::styled("installed", Style::default().fg(Color::Green))),
            Some(false) => spans.push(Span::styled(
                "not installed",
                Style::default().fg(Color::Red),
            )),
            None => {}
        }
        if *runtime == p.default {
            spans.push(Span::styled(
                if p.switch {
                    "  (current)"
                } else {
                    "  (default)"
                },
                dim,
            ));
        }
        out.push(Line::from(spans));
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(
        if p.switch {
            "↑↓ or 1-9 pick · Enter choose · Esc cancel"
        } else {
            "↑↓ or 1-9 pick · Enter choose · Esc decide later"
        },
        dim,
    )));
    out
}

/// The orchestrator picker, centered over the view.
fn render_picker(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let lines = picker_lines(state);
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    // Wrapped here to the width it gets (a phone is narrower than its heading), so the rows
    // of each runtime are known: a tap on them picks it.
    let inner_w = usize::from(width.min(area.width).saturating_sub(2)).max(1);
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut spans = Vec::new();
    for line in lines {
        let start = rows.len();
        rows.extend(wrap_line(line, inner_w));
        spans.push(start..rows.len());
    }
    let rect = centered(
        area,
        width,
        u16::try_from(rows.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2),
    );
    let switch = state.view.picker.as_ref().is_some_and(|p| p.switch);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(if switch {
            " switch the orchestrator "
        } else {
            " choose the orchestrator "
        })
        .title(close_title());
    let inner = block.inner(rect);
    panes.cover(None);
    panes.add(PaneId::Text, inner);
    close_row(panes, rect, Target::Close);
    // Its runtimes, from the fourth line ([`picker_lines`]): the rows each wrapped into.
    let runtimes = state.view.picker.as_ref().map_or(0, |p| p.runtimes.len());
    for (i, span) in spans.iter().skip(3).take(runtimes).enumerate() {
        let y = inner
            .y
            .saturating_add(u16::try_from(span.start).unwrap_or(u16::MAX));
        let h = u16::try_from(span.len()).unwrap_or(u16::MAX);
        let cells = Rect::new(inner.x, y, inner.width, h).intersection(inner);
        panes.target(cells, Target::Runtime(i));
    }
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(rows).block(block), rect);
}

/// The stack view: the main pane, its input box and status bar, and the worker cards beside
/// or above it, unless `hide_cards` (the compact layout's conversation).
fn render_stack(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    panes: &mut Panes,
    hide_cards: bool,
) {
    let [upper, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(status_height(state, area)),
    ])
    .areas(area);
    // Too narrow for a column of cards, a dive takes the whole height: its header band names
    // the thread and says Esc gets back, to the row of cards as it was.
    let narrow_dive = upper.width < SIDE_MIN_WIDTH && state.view.dive.is_some();
    let (left, side) = if !hide_cards && !state.view.sidebar_hidden && upper.width >= SIDE_MIN_WIDTH
    {
        let [left, side] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(side_width(upper.width)),
        ])
        .areas(upper);
        (left, Some(side))
    } else {
        (upper, None)
    };
    // The input box is under the main pane only: the column of cards beside it keeps its
    // height however many rows the box grows to.
    let [top, input] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(input_height(
            state,
            Rect {
                width: left.width,
                ..area
            },
        )),
    ])
    .areas(left);
    let noted = if let Some(side) = side {
        // The cards first: the main pane shows a confirm card the hub's card cut short
        // ([`hub_detail`]).
        let noted = render_card_column(frame, side, state, panes);
        render_main(frame, top, state, panes);
        noted
    } else if hide_cards || state.view.sidebar_hidden || narrow_dive {
        render_main(frame, top, state, panes);
        false
    } else {
        let row = if state.order().is_empty() {
            3
        } else {
            CARD_HEIGHT
        };
        let [cards, main] =
            Layout::vertical([Constraint::Length(row), Constraint::Min(3)]).areas(top);
        let noted = render_cards(frame, cards, state, panes);
        render_main(frame, main, state, panes);
        noted
    };
    render_input(frame, input, state, panes);
    // The note on hidden parked threads the card area had no room for leads the hints.
    let parked = parked_note(state).filter(|_| !noted);
    render_status(frame, status, state, panes, parked);
}

/// The compact layout (a phone): a line naming its screens, `Chat · Needs you 2 · Threads`,
/// the current one highlighted, over that screen: the conversation alone, what needs the
/// user, or the threads, each list one line per row with the input box and status bar under it.
fn render_compact(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    use crate::app::CompactPane;
    let [bar, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let pane = state.view.compact_pane;
    let needs = state.needs_count();
    let names = [
        (CompactPane::Chat, "Chat".to_string()),
        (
            CompactPane::Needs,
            if needs > 0 {
                format!("Needs you {needs}")
            } else {
                "Needs you".to_string()
            },
        ),
        (CompactPane::Threads, "Threads".to_string()),
    ];
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    // Where each name starts: a tap on it, or on the separator after it, shows its screen,
    // the last one's reaching the end of the line, so the whole row is tappable.
    let mut starts = Vec::new();
    let mut x = bar.x;
    for (i, (p, name)) in names.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim));
            x = x.saturating_add(3);
        }
        starts.push((*p, x));
        x = x.saturating_add(crate::edit::width(name) as u16);
        let style = if *p == pane {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else if *p == CompactPane::Needs && needs > 0 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        spans.push(Span::styled(name.clone(), style));
    }
    panes.add(PaneId::Text, bar);
    for (k, (p, start)) in starts.iter().enumerate() {
        let from = if k == 0 {
            bar.x
        } else {
            start.saturating_sub(1)
        };
        let to = starts
            .get(k + 1)
            .map_or(bar.right(), |(_, next)| next.saturating_sub(1));
        let cells = Rect::new(from, bar.y, to.saturating_sub(from), 1).intersection(bar);
        panes.target(cells, Target::Screen(*p));
    }
    frame.render_widget(
        Paragraph::new(Line::from(cut_spans(spans, usize::from(bar.width)))),
        bar,
    );
    if pane == CompactPane::Chat {
        render_stack(frame, rest, state, panes, true);
        return;
    }
    let [list, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height(state, rest)),
        Constraint::Length(status_height(state, rest)),
    ])
    .areas(rest);
    let rows = if pane == CompactPane::Needs {
        compact_needs_rows(state, usize::from(list.width))
    } else {
        compact_thread_rows(state, usize::from(list.width))
    };
    render_compact_rows(frame, list, state, panes, rows);
    render_input(frame, input, state, panes);
    render_status(frame, status, state, panes, None);
}

/// A compact list's rows: the card each is about, and its line.
type CompactRows = Vec<(String, Line<'static>)>;

/// "Needs you": one line per card that waits on the user, its most urgent item,
/// `api · permission · Bash: git push`, and `(+2 more)` when it has more (answering one shows
/// the next), most urgent first.
fn compact_needs_rows(state: &AppState, width: usize) -> CompactRows {
    use twr_core::event::AttentionReason;
    let mut rows = Vec::new();
    for id in state.needs_order() {
        let items = state.card_items(&id);
        let Some(item) = items.first() else {
            continue;
        };
        let name = compact_name(state, &id);
        let kind = if item.proposal {
            "confirm"
        } else {
            match item.reason {
                AttentionReason::Permission => "permission",
                AttentionReason::Question => "question",
                AttentionReason::Error => "error",
                AttentionReason::Review => "review",
                AttentionReason::Stalled => "stalled",
            }
        };
        let color = if item.reason.is_blocking() || item.proposal {
            Color::Yellow
        } else {
            Color::Red
        };
        let summary: String = item
            .summary
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut spans = vec![
            Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" · "),
            Span::styled(kind, Style::default().fg(color)),
        ];
        if items.len() > 1 {
            spans.push(Span::styled(
                format!(" (+{} more)", items.len() - 1),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.push(Span::raw(" · "));
        spans.push(Span::raw(summary));
        rows.push((id, Line::from(cut_spans(spans, width))));
    }
    rows
}

/// "Threads": one line per thread, `api · running · editing src/main.rs`.
fn compact_thread_rows(state: &AppState, width: usize) -> CompactRows {
    state
        .compact_threads()
        .into_iter()
        .map(|id| {
            let name = compact_name(state, &id);
            let status = state.registry.get(&id).map(card_status).unwrap_or_default();
            let activity = state
                .activity
                .get(&id)
                .map(|a| a.activity.clone())
                .unwrap_or_default();
            let mut spans = vec![
                Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" · "),
                Span::styled(status, Style::default().fg(Color::Cyan)),
            ];
            if !activity.is_empty() {
                spans.push(Span::raw(" · "));
                spans.push(Span::styled(activity, Style::default().fg(Color::DarkGray)));
            }
            (id, Line::from(cut_spans(spans, width)))
        })
        .collect()
}

/// What a compact list calls card `id`: its thread's name, `twr` for the hub's card,
/// `orchestrator` for the orchestrator's banner.
fn compact_name(state: &AppState, id: &str) -> String {
    if id == twr_core::event::HUB_THREAD {
        return "twr".into();
    }
    match state.registry.get(id) {
        Some(v) if v.is_orchestrator() => "orchestrator".into(),
        Some(v) => v.name.clone(),
        None => id.to_string(),
    }
}

/// Draws a compact list in `area`: the selected card's rows highlighted and kept in view, each
/// row a pane of its card (a click selects it); a note when there is none.
fn render_compact_rows(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    panes: &mut Panes,
    rows: CompactRows,
) {
    use crate::app::CompactPane;
    if rows.is_empty() {
        let note = if state.view.compact_pane == CompactPane::Needs {
            "nothing waits on you"
        } else {
            EMPTY_STATE
        };
        panes.add(PaneId::Text, area);
        frame.render_widget(
            Paragraph::new(Line::styled(note, Style::default().fg(Color::DarkGray)))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let height = usize::from(area.height).max(1);
    let focus = state.focused();
    // Scrolled like the cards: by the wheel, else as little as the selection takes.
    let order: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
    let focus_idx = focus.and_then(|f| order.iter().position(|id| id == f));
    let max = order.len().saturating_sub(height);
    let fresh = focus_idx.map_or(0, |f| (f + 1).saturating_sub(height).min(max));
    let layout = ListLayout::Compact(state.view.compact_pane);
    let shown = |start: usize| order.len().saturating_sub(start).min(height);
    let start = list_start(state, layout, &order, 1, focus_idx, fresh, max, shown);
    panes.list_first = order.get(start).cloned();
    panes.list = Some(ListWindow {
        layout,
        area,
        start,
        max,
    });
    for (i, (id, line)) in rows.into_iter().skip(start).take(height).enumerate() {
        let rect = Rect::new(area.x, area.y + i as u16, area.width, 1);
        let mut line = line;
        if Some(id.as_str()) == focus {
            let pad = usize::from(area.width).saturating_sub(line.width());
            line.spans.push(Span::raw(" ".repeat(pad)));
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
        panes.add(PaneId::Thread(id), rect);
        frame.render_widget(Paragraph::new(line), rect);
    }
}

/// Width of the input box on a screen `width` columns wide: in the stack view with the cards
/// in a column, the main pane's width (the column runs down beside the box), else the whole
/// width.
pub fn input_box_width(state: &AppState, width: u16) -> u16 {
    if state.compact() {
        return width;
    }
    if state.view.screen == Screen::Stack && !state.view.sidebar_hidden && width >= SIDE_MIN_WIDTH {
        width.saturating_sub(side_width(width))
    } else {
        width
    }
}

/// Which slice of `n` cards to show in `width` columns so that `focus` stays visible.
/// Returns `(start, count)`.
pub fn visible_window(n: usize, focus: Option<usize>, width: u16) -> (usize, usize) {
    let fit = usize::from((width / CARD_MIN_WIDTH).max(1));
    let count = fit.min(n);
    let focus = focus.unwrap_or(0);
    let start = if count == 0 {
        0
    } else {
        (focus + 1).saturating_sub(count)
    };
    (start, count)
}

/// Which slice of cards `heights` tall to show in a column `height` rows tall so that `focus`
/// stays visible: the first card shown and how many fit (at least one). Returns `(start,
/// count)`.
pub fn column_window(heights: &[u16], focus: Option<usize>, height: u16) -> (usize, usize) {
    let n = heights.len();
    if n == 0 {
        return (0, 0);
    }
    let focus = focus.unwrap_or(0).min(n - 1);
    let sum = |a: usize, b: usize| heights[a..b].iter().map(|h| u32::from(*h)).sum::<u32>();
    let height = u32::from(height);
    let mut start = 0;
    while start < focus && sum(start, focus + 1) > height {
        start += 1;
    }
    (start, column_count(heights, start, height))
}

/// How many of the cards `heights` tall fit in `height` rows from card `start` on (at least
/// one).
fn column_count(heights: &[u16], start: usize, height: u32) -> usize {
    let n = heights.len();
    let sum = |a: usize, b: usize| heights[a..b].iter().map(|h| u32::from(*h)).sum::<u32>();
    let mut count = 1;
    while start + count < n && sum(start, start + count + 1) <= height {
        count += 1;
    }
    count.min(n.saturating_sub(start))
}

/// The furthest a column of cards `heights` tall scrolls in `height` rows: the first card from
/// which the last ones all fit (the last card alone when it does not).
fn column_max(heights: &[u16], height: u16) -> usize {
    let mut rows = 0u32;
    let mut start = heights.len();
    while start > 0 && rows + u32::from(heights[start - 1]) <= u32::from(height) {
        start -= 1;
        rows += u32::from(heights[start]);
    }
    start.min(heights.len().saturating_sub(1))
}

/// The first card (in the grid, the first row of tiles) of list `layout` to show, at most
/// `max` (from which the last cards show): where the wheel scrolled it
/// ([`crate::app::ViewState::card_scroll`]) while the selection is still the one it was
/// scrolled with; else the slice the last frame showed while the selection (`focus`, its
/// index) is in it (`shown(start)`: how many show from `start`), moved as little as it takes
/// to show it when not: up to it, or down to `fresh`, the first slice it is the last of.
/// A list scrolled down (from its first card on) keeps showing first the card (the row of
/// the tile) it showed first, when cards came or went above it: the slice does not jump.
/// `order` is the list's cards, `per` how many there are to a row (1 but in the grid).
#[allow(clippy::too_many_arguments)]
fn list_start(
    state: &AppState,
    layout: ListLayout,
    order: &[String],
    per: usize,
    focus: Option<usize>,
    fresh: usize,
    max: usize,
    shown: impl Fn(usize) -> usize,
) -> usize {
    // The anchored card's place now: past `max` too (the room below it left empty), so that
    // a card coming above it near the end does not push it down either.
    let per = per.max(1);
    let last_row = order.len().saturating_sub(1) / per;
    let at = |start: usize, first: Option<&str>| {
        first
            .filter(|_| start > 0)
            .and_then(|f| order.iter().position(|id| id == f))
            .map_or(start.min(max), |i| (i / per).min(last_row))
    };
    if let Some(s) = state
        .view
        .card_scroll
        .as_ref()
        .filter(|s| s.layout == layout && s.focus.as_deref() == state.focused())
    {
        return at(s.start, s.first.as_deref());
    }
    let last = state
        .view
        .panes
        .list
        .filter(|l| l.layout == layout)
        .map(|l| at(l.start, state.view.panes.list_first.as_deref()));
    // `last` is within the list already (past `max` only to keep its first card first).
    match (last, focus) {
        (None, _) => fresh,
        (Some(last), Some(f)) if f < last => f,
        (Some(last), Some(f)) if f < last + shown(last) => return last,
        (Some(_), Some(_)) => fresh,
        (Some(last), None) => return last,
    }
    .min(max)
}

/// The "no worker threads" pane whose inside is `inner`, with `note` under the message when
/// the note fits there whole; also returns whether it does.
fn empty_state(
    inner: Rect,
    note: Option<String>,
    focus: bool,
    instance: Option<&str>,
) -> (Paragraph<'static>, bool) {
    let width = usize::from(inner.width).max(1);
    let rows = usize::from(inner.height);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let message = if focus {
        "no threads working or needing you · Ctrl-F shows all"
    } else {
        EMPTY_STATE
    };
    let mut text: Vec<Line<'static>> = wrap(message, width)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, bold)))
        .collect();
    let note = note
        .map(|n| wrap(&n, width))
        .filter(|n| text.len() + n.len() <= rows);
    let noted = note.is_some();
    for l in note.into_iter().flatten() {
        text.push(Line::from(Span::styled(
            l,
            Style::default().fg(Color::DarkGray),
        )));
    }
    // Centered vertically when there is room.
    let mut lines = vec![Line::default(); rows.saturating_sub(text.len()).min(2)];
    lines.extend(text);
    let pane = Paragraph::new(lines).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(match instance {
                Some(name) => format!(" worker threads · [{}] ", truncate_cells(name, width / 3)),
                None => " worker threads ".into(),
            }),
    );
    (pane, noted)
}

/// Draws the "no worker threads" pane over `area`, with the hidden parked threads' note
/// ([`parked_note`]) when it fits; returns whether it is drawn.
fn render_empty(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) -> bool {
    let inner = Block::default().borders(Borders::ALL).inner(area);
    panes.add(PaneId::Text, inner);
    // Nothing listed in Focus mode: a tap shows all (Ctrl-F).
    if state.view.focus_mode {
        panes.target(inner, Target::FocusOff);
    }
    let (pane, noted) = empty_state(
        inner,
        parked_note(state),
        state.view.focus_mode,
        state.instance_label(),
    );
    frame.render_widget(pane, area);
    // A tap on the parked note lists them (`/parked`), Focus mode's control around it.
    if noted {
        if let Some(cells) = find_text(frame.buffer_mut(), inner, "/parked") {
            panes.target(Rect::new(inner.x, cells.y, inner.width, 1), Target::Parked);
        }
    }
    noted
}

/// `2 parked hidden · /parked shows them` while the card listing leaves parked threads out
/// ([`AppState::hidden_parked`]).
pub fn parked_note(state: &AppState) -> Option<String> {
    let n = state.hidden_parked().len();
    (n > 0).then(|| format!("{n} parked hidden · /parked shows them"))
}

/// The card of `id` (a worker or [`HUB_THREAD`]) at `slot`, with the `more` labels on its
/// bottom border (left, right). The card's pane is recorded for the mouse.
fn draw_card(
    frame: &mut Frame,
    slot: Rect,
    state: &AppState,
    id: &str,
    more: (Option<String>, Option<String>),
    panes: &mut Panes,
) {
    let inner = Block::default().borders(Borders::ALL).inner(slot);
    panes.add(PaneId::Thread(id.to_string()), inner);
    let look = look(state, id);
    let drawn = if id == HUB_THREAD {
        render_hub_card(frame, slot, state, look, more)
    } else if let Some(thread) = state.registry.get(id) {
        render_card(frame, slot, state, thread, look, more, panes)
    } else {
        None
    };
    record(panes, id, drawn, inner);
}

/// Records that the card of `thread` over `inner` showed attention item `drawn` (id and
/// revision) in full, if it did.
fn record(panes: &mut Panes, thread: &str, drawn: Option<(String, u64)>, inner: Rect) {
    if let Some((attn_id, revision)) = drawn {
        panes.drawn(DrawnItem {
            thread: thread.to_string(),
            attn_id,
            revision,
            area: inner,
        });
    }
}

/// The worker cards in a row (narrow terminals), as many as fit at [`CARD_MIN_WIDTH`].
/// Returns whether the note on hidden parked threads ([`parked_note`]) is drawn: only the
/// empty pane has room for it.
fn render_cards(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) -> bool {
    let order = state.order();
    if order.is_empty() {
        return render_empty(frame, area, state, panes);
    }
    let focus_idx = kept_in_view(state).and_then(|f| order.iter().position(|id| id == f));
    let (fresh, count) = visible_window(order.len(), focus_idx, area.width);
    let max = order.len() - count;
    let layout = ListLayout::Row;
    let start = list_start(state, layout, &order, 1, focus_idx, fresh, max, |_| count);
    panes.list_first = order.get(start).cloned();
    panes.list = Some(ListWindow {
        layout,
        area,
        start,
        max,
    });
    let slots = Layout::horizontal(vec![Constraint::Ratio(1, count as u32); count]).split(area);
    for (slot, (i, id)) in slots
        .iter()
        .zip(order.iter().enumerate().skip(start).take(count))
    {
        let left = (i == start && start > 0).then(|| format!("< {start} more"));
        let right = (i + 1 == start + count && start + count < order.len())
            .then(|| format!("{} more >", order.len() - i - 1));
        draw_card(frame, *slot, state, id, (left, right), panes);
    }
    false
}

/// The card the row or column of cards keeps in view: the selected one, else, with none
/// selected, the thread dived into (where typed lines go).
fn kept_in_view(state: &AppState) -> Option<&str> {
    state.focused().or(state.view.dive.as_deref())
}

/// Height of `id`'s card in a side column: [`CARD_HEIGHT`] for the hub's card and a card
/// with an item (or shown as needing you until the order is next applied,
/// [`AppState::card_tall`]), else [`COMPACT_CARD_HEIGHT`]. Fixed whatever its text says, so
/// that text changing (its activity, its report, an item's options) never moves the cards
/// below it; what does not fit is cut ([`card_rows`]).
fn card_height(state: &AppState, id: &str) -> u16 {
    if id == HUB_THREAD || state.card_tall(id) {
        CARD_HEIGHT
    } else {
        COMPACT_CARD_HEIGHT
    }
}

/// The worker cards in a column beside the main pane, as many as fit, keeping the selected one
/// in view unless the wheel scrolled them ([`list_start`]); under them a dim row says how many
/// parked threads are hidden ([`parked_note`]).
/// Returns whether that note is drawn.
fn render_card_column(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) -> bool {
    let order = state.order();
    if order.is_empty() {
        return render_empty(frame, area, state, panes);
    }
    let column = area;
    let mut area = area;
    // Drawn on a row of its own after a space, when it fits there whole.
    let note = parked_note(state)
        .filter(|n| area.height > 3 && n.chars().count() < usize::from(area.width));
    let noted = note.is_some();
    if let Some(note) = note {
        let [cards, row] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {note}"),
                Style::default().fg(Color::DarkGray),
            ))),
            row,
        );
        panes.add(PaneId::Text, row);
        // Its row lists them (`/parked`).
        panes.target(row, Target::Parked);
        area = cards;
    }
    let heights: Vec<u16> = order.iter().map(|id| card_height(state, id)).collect();
    let focus_idx = kept_in_view(state).and_then(|f| order.iter().position(|id| id == f));
    let (fresh, _) = column_window(&heights, focus_idx, area.height);
    let max = column_max(&heights, area.height);
    let layout = ListLayout::Column;
    let fits = |start: usize| column_count(&heights, start, u32::from(area.height));
    let start = list_start(state, layout, &order, 1, focus_idx, fresh, max, fits);
    let count = fits(start);
    panes.list_first = order.get(start).cloned();
    panes.list = Some(ListWindow {
        layout,
        area: column,
        start,
        max,
    });
    let mut y = area.y;
    for i in start..start + count {
        let height = heights[i].min(area.bottom().saturating_sub(y));
        if height < 3 {
            break;
        }
        let slot = Rect::new(area.x, y, area.width, height);
        let above = (i == start && start > 0).then(|| format!("↑ {start} more"));
        let below = (i + 1 == start + count && start + count < order.len())
            .then(|| format!("↓ {} more", order.len() - i - 1));
        draw_card(frame, slot, state, &order[i], (above, below), panes);
        y += height;
    }
    noted
}

/// `s` on one row: its whitespace runs, line breaks included, as one space.
fn one_row(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The card or tile drawn selected: the selected one, unless the main pane is
/// ([`AppState::main_focused`]) or the selection is only the one a narrow dive puts on its
/// hidden card ([`AppState::forced_focus`]). Typing goes on whatever is drawn selected: where
/// the line goes is drawn in its own color ([`target_color`]: the main pane's thick border, the
/// card of the thread dived into, [`Look::Target`]).
fn highlighted(state: &AppState) -> Option<&str> {
    if state.main_focused() || state.forced_focus() {
        None
    } else {
        state.focused()
    }
}

/// How a card or tile is drawn: selected (a thick yellow border), the thread dived into, where
/// typed lines go (a thin border in the dive's color: told apart from the selection by both),
/// or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Plain,
    Target,
    Selected,
}

/// How the card or tile of `id` is drawn ([`Look`]): selected wins over where text goes.
fn look(state: &AppState, id: &str) -> Look {
    if highlighted(state) == Some(id) {
        Look::Selected
    } else if state.view.dive.as_deref() == Some(id) {
        Look::Target
    } else {
        Look::Plain
    }
}

/// `block` with the border of a card or tile drawn `look`, `plain` its border otherwise.
fn card_border(block: Block<'static>, look: Look, plain: Style) -> Block<'static> {
    match look {
        Look::Selected => block
            .border_type(BorderType::Thick)
            .border_style(Style::default().fg(Color::Yellow)),
        Look::Target => block.border_style(Style::default().fg(target_color(true))),
        Look::Plain => block.border_style(plain),
    }
}

/// What `t` is doing now on one row `width` cells wide, cut with `…`: on every card and tile,
/// under its status. The hub's activity line (`thread.activity`), else, until the hub said,
/// what its status says ([`twr_core::activity::fallback`]).
fn activity_line(state: &AppState, t: &ThreadView, width: usize) -> Line<'static> {
    let line = state
        .activity
        .get(&t.id)
        .map(|a| a.activity.as_str())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| twr_core::activity::fallback(t));
    Line::from(Span::styled(
        truncate_cells(&format!("› {line}"), width),
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// The title of a card or tile `width` columns wide: its project, dim, then its name (`twr ·
/// scroll-follow`), `[pin]`, and the permission mode on the right when both fit. The project
/// gives way first: cut with `…` to the room the rest leaves ([`title_project`]), then left
/// out.
fn card_block(t: &ThreadView, pinned: bool, width: u16) -> Block<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let width = usize::from(width);
    let mut title = vec![Span::styled(
        format!(" {} ", t.name),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if pinned {
        title.push(Span::styled("[pin] ", Style::default().fg(Color::Cyan)));
    }
    let used = Line::from(title.clone()).width();
    let mode = format!(" {} ", t.mode);
    let mode_width = mode.chars().count();
    let show_mode = !t.mode.is_empty() && used + mode_width + 4 <= width;
    let room = width.saturating_sub(used + 4 + if show_mode { mode_width } else { 0 });
    if let Some(project) = title_project(t.project.as_deref(), room) {
        title[0].content = format!("{} ", t.name).into();
        title.insert(0, Span::styled(format!(" {project} · "), dim));
    }
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title));
    if show_mode {
        block = block.title(Line::from(Span::styled(mode, dim)).right_aligned());
    }
    block
}

/// The project a card's or tile's title shows before the thread's name, with `room` cells for
/// it and the ` · ` after it: whole when it fits, else cut with `…` to at least two of its
/// characters; none without a project or the room.
pub fn title_project(project: Option<&str>, room: usize) -> Option<String> {
    let project = project.map(str::trim).filter(|p| !p.is_empty())?;
    let room = room.checked_sub(3)?;
    if crate::edit::width(project) <= room {
        return Some(project.to_string());
    }
    (room >= 3).then(|| truncate_cells(project, room))
}

/// Draws a worker's card (its pane is recorded already); returns the attention item it showed
/// in full (id and revision).
fn render_card(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    t: &ThreadView,
    look: Look,
    (more_left, more_right): (Option<String>, Option<String>),
    panes: &mut Panes,
) -> Option<(String, u64)> {
    let pinned = state.view.pins.contains(&t.id);
    let block = card_block(t, pinned, area.width);
    let mut block = card_border(block, look, Style::default().fg(Color::DarkGray));
    if let Some(l) = more_left {
        block = block.title_bottom(Line::from(l).left_aligned());
    }
    if let Some(r) = more_right {
        block = block.title_bottom(Line::from(r).right_aligned());
    }

    let inner = block.inner(area);
    let (body, attn) = card_parts(state, t, usize::from(inner.width));
    let status_row = body.first().filter(|_| icon::is_working(t)).cloned();
    let (rows, drawn) = card_rows(body, attn, inner.height, inner.width);
    if let Some(cells) = shows_icon(&rows, status_row.as_ref()) {
        note_icon(panes, state, &t.id, inner, cells);
    }
    frame.render_widget(
        Paragraph::new(rows).block(block).style(parked_style(t)),
        area,
    );
    drawn
}

/// The cells to leave out of copied text when `rows`, as drawn, start with the working icon
/// `status_row` (a card's or tile's status row, when its thread works) leads with: the icon,
/// and the space after it when that is drawn too. A row cut short keeps the icon; one given
/// way to `…` in a cell too narrow does not, and a row cut right after it has its `…` there.
fn shows_icon(rows: &[Line<'static>], status_row: Option<&Line<'static>>) -> Option<u16> {
    let row = rows.first()?;
    let icon = status_row?.spans.first()?;
    (row.spans.first().map(|s| &s.content) == Some(&icon.content)).then(|| {
        let space = row.spans.get(1).is_some_and(|s| s.content == " ");
        icon::WIDTH + u16::from(space)
    })
}

/// The working icon of `t` for this draw ([`icon::cursor`]: the frame of the clock, or the
/// still `_` without `[tui] animate`), if it is working ([`icon::is_working`]).
fn working_icon(state: &AppState, t: &ThreadView) -> Option<Span<'static>> {
    icon::is_working(t).then(|| icon::cursor(state.view.animate.then_some(state.view.frame)))
}

/// Notes the working icon drawn at the top left of the card or tile of `thread` over `inner`
/// (the status row starts with it): its first `cells` (the icon, and the space after it when
/// that is drawn, [`shows_icon`]) are left out of copied text, and its cursor, when it moves
/// and shows, keeps the clock ticking ([`Panes::moving`]).
fn note_icon(panes: &mut Panes, state: &AppState, thread: &str, inner: Rect, cells: u16) {
    let width = cells.min(inner.width);
    if inner.height == 0 || width == 0 {
        return;
    }
    panes.skip(
        &PaneId::Thread(thread.to_string()),
        Rect::new(inner.x, inner.y, width, 1),
    );
    if state.view.animate && inner.width >= icon::WIDTH {
        panes.moves(Rect::new(inner.x + icon::WIDTH - 1, inner.y, 1, 1));
    }
}

/// The text of a worker's card `width` cells wide inside, in rows: status (with its report's
/// grade, [`card_status`]), runtime and model; git; then one slot ([`slot_lines`]): apart, the
/// top attention item ([`attention_rows`]), or else what it is doing or what it came to. (Its
/// project is in its title, [`card_block`].)
fn card_parts(
    state: &AppState,
    t: &ThreadView,
    width: usize,
) -> (Vec<Line<'static>>, Option<Arc<AttnRows>>) {
    let runtime = if t.runtime.is_empty() {
        "?".to_string()
    } else {
        t.runtime.clone()
    };
    let mut lines = status_lines(t, runtime, width, working_icon(state, t));
    // The git state on one row, cut with `…`: the slot keeps the rows the card has.
    if let Some(g) = &t.git {
        lines.push(Line::from(cut_spans(git_line(g).spans, width)));
    }
    let attn = attention_rows(state, &t.id, width, false);
    if attn.is_none() {
        lines.extend(slot_lines(state, t, width));
    }
    (lines, attn)
}

/// Whether `t` is at work: a turn running, background tasks left, a request open or its CLI
/// attached. Its card's slot shows what it does then, not a report of a turn before.
fn at_work(t: &ThreadView) -> bool {
    matches!(
        t.status(),
        ThreadStatus::Running
            | ThreadStatus::Background
            | ThreadStatus::Waiting
            | ThreadStatus::Attached
    )
}

/// The slot of a card or tile `width` wide with no item open: while its thread is at work
/// ([`at_work`]) or failed, the activity line (one row: what it does now, why it failed); else
/// its last report (dimmed, wrapped; the card cuts it to the rows it has, with `…`), or the
/// activity line when it has none (`not started`, `idle`). A selected card waiting on
/// background work shows the report its turn yielded with under the activity: an interim one.
fn slot_lines(state: &AppState, t: &ThreadView, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let report = |r: &twr_core::event::ReportReady| {
        wrap(&r.summary, width.max(1))
            .into_iter()
            .map(|l| Line::from(Span::styled(l, dim)))
            .collect::<Vec<_>>()
    };
    let status = t.status();
    if !at_work(t) && status != ThreadStatus::Error {
        if let Some(r) = &t.last_report {
            return report(r);
        }
    }
    let mut lines = vec![activity_line(state, t, width)];
    let selected = highlighted(state) == Some(t.id.as_str());
    if selected && status == ThreadStatus::Background && t.reported {
        lines.extend(t.last_report.as_ref().map(report).unwrap_or_default());
    }
    lines
}

/// The status a card or tile shows: the thread's (`failed` or `crashed` for an error,
/// `stopped` for a stopped one), with the grade of the report on its last turn once it is
/// done (`done ✓`: success, `~` partial, `✗` gave up, `?` needs verification), and its
/// background tasks (`running (1 bg)`).
fn card_status(t: &ThreadView) -> String {
    use twr_core::event::Grade;
    let status = t.status();
    let word = match status {
        ThreadStatus::Error if t.crashed => "crashed".to_string(),
        ThreadStatus::Error => "failed".to_string(),
        ThreadStatus::Done if t.stopped => "stopped".to_string(),
        ThreadStatus::Done => match t.last_report.as_ref().filter(|_| t.reported) {
            Some(r) => format!(
                "done {}",
                match r.grade {
                    Grade::Success => "✓",
                    Grade::Partial => "~",
                    Grade::GaveUp => "✗",
                    Grade::NeedsVerification => "?",
                }
            ),
            None => "done".to_string(),
        },
        s => s.as_str().to_string(),
    };
    match t.background_tasks.len() {
        0 => word,
        n => format!("{word} ({n} bg)"),
    }
}

/// The rows a card `height` rows tall and `width` wide shows of `body` and `attn`
/// ([`fit_rows`]), and the item it shows in full (id and revision), if any. A body cut short
/// (a long report) ends in `…`.
fn card_rows(
    body: Vec<Line<'static>>,
    attn: Option<Arc<AttnRows>>,
    height: u16,
    width: u16,
) -> (Vec<Line<'static>>, Option<(String, u64)>) {
    let height = usize::from(height);
    match attn {
        Some(a) => {
            let (rows, whole) = fit_rows(body, &a.core, &a.extra, height);
            (
                rows,
                (whole && a.exact && a.complete).then(|| a.item.clone()),
            )
        }
        None => {
            let cut = body.len() > height;
            let mut rows: Vec<Line<'static>> = body.into_iter().take(height).collect();
            if let Some(last) = rows.last_mut().filter(|_| cut) {
                *last = ellipsized(std::mem::take(last), usize::from(width));
            }
            (rows, None)
        }
    }
}

/// `line` ending in `…`, within `width` cells: what comes after it was cut.
fn ellipsized(line: Line<'static>, width: usize) -> Line<'static> {
    let style = line.spans.last().map(|s| s.style).unwrap_or_default();
    let mut spans = line.spans;
    spans.push(Span::styled("…", style));
    // Too wide with it: cut, which ends in `…` itself.
    Line::from(cut_spans(spans, width))
}

/// The rows of a card `height` rows tall: `body`, then `core` (an attention item: its badge
/// and summary, then its options), then `extra` (its note, how many more wait). When they do
/// not all fit, `core` is kept whole first, then `extra`, and the body gives way from its end.
/// Returns the rows and whether `core` is drawn whole. Only the rows drawn are copied: an
/// item's rows are kept between frames ([`WrapCache`]) and may be many.
fn fit_rows(
    body: Vec<Line<'static>>,
    core: &[Line<'static>],
    extra: &[Line<'static>],
    height: usize,
) -> (Vec<Line<'static>>, bool) {
    if core.len() > height {
        return (core[..height].to_vec(), false);
    }
    let room = height - core.len();
    let extra_n = extra.len().min(room);
    let body_n = body.len().min(room - extra_n);
    let mut rows: Vec<Line<'static>> = body.into_iter().take(body_n).collect();
    rows.extend_from_slice(core);
    rows.extend_from_slice(&extra[..extra_n]);
    (rows, true)
}

/// `line` in rows of at most `width` cells: each of its hard line breaks (`\n`, as between the
/// threads of a delete card's summary) starts a row, and each part is broken at spaces where it
/// can (a space at a break is not drawn), inside a word longer than a row (between its
/// grapheme clusters, never inside one: a joined emoji stays whole). Drawn without
/// wrapping, each row shows in full (but for a character wider than a row: see
/// [`AttnRows::exact`]).
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    wrap_joined(line, width)
        .into_iter()
        .map(|(l, _)| l)
        .collect()
}

/// [`wrap_line`], each row with how it continues the row above ([`Join`]): the first row of
/// each of its lines starts one, a row broken at a space continues it after a space, one
/// broken inside a word right after it.
fn wrap_joined(line: Line<'static>, width: usize) -> Vec<(Line<'static>, Join)> {
    if has_breaks(&line) {
        return hard_lines(line)
            .into_iter()
            .flat_map(|l| wrap_joined(l, width))
            .collect();
    }
    let width = width.max(1);
    let style = line.style;
    // Whole grapheme clusters ([`clusters`]), a cluster across spans in the style of the one
    // it starts in.
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut ends = Vec::with_capacity(line.spans.len());
    let mut end = 0;
    for s in &line.spans {
        end += s.content.len();
        ends.push((end, s.style));
    }
    let cells: Vec<(&str, Style)> = clusters(&text)
        .into_iter()
        .map(|(at, g)| {
            let k = ends.partition_point(|&(end, _)| end <= at);
            (g, ends.get(k).map_or(Style::default(), |&(_, st)| st))
        })
        .collect();
    if cells.iter().map(|(g, _)| cluster_width(g)).sum::<usize>() <= width {
        return vec![(line, Join::Newline)];
    }
    let mut rows: Vec<(Vec<(&str, Style)>, Join)> = Vec::new();
    let mut row: Vec<(&str, Style)> = Vec::new();
    let mut join = Join::Newline;
    let mut used = 0;
    let mut i = 0;
    while i < cells.len() {
        if cells[i].0 == " " {
            if used + 1 > width {
                rows.push((std::mem::take(&mut row), join));
                join = Join::Space;
                used = 0;
            } else {
                row.push(cells[i]);
                used += 1;
            }
            i += 1;
            continue;
        }
        let end = (i..cells.len())
            .find(|&k| cells[k].0 == " ")
            .unwrap_or(cells.len());
        let word = &cells[i..end];
        let w: usize = word.iter().map(|(g, _)| cluster_width(g)).sum();
        if used > 0 && used + w > width {
            while row.last().is_some_and(|(g, _)| *g == " ") {
                row.pop();
            }
            rows.push((std::mem::take(&mut row), join));
            join = Join::Space;
            used = 0;
        }
        for &(g, st) in word {
            let gw = cluster_width(g);
            if used > 0 && used + gw > width {
                rows.push((std::mem::take(&mut row), join));
                join = Join::Glue;
                used = 0;
            }
            row.push((g, st));
            used += gw;
        }
        i = end;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push((row, join));
    }
    rows.into_iter()
        .map(|(r, join)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut text = String::new();
            let mut cur: Option<Style> = None;
            for (g, st) in r {
                if let Some(prev) = cur.filter(|p| *p != st) {
                    spans.push(Span::styled(std::mem::take(&mut text), prev));
                }
                cur = Some(st);
                text.push_str(g);
            }
            if let Some(st) = cur {
                spans.push(Span::styled(text, st));
            }
            (Line::from(spans).style(style), join)
        })
        .collect()
}

/// `line` in rows of `width` cells, broken wherever a row is full ([`Join::Glue`]), never at
/// a space: every space is kept where it is, none dropped at a row's end or start (word
/// wrapping, [`wrap_joined`], drops them). For text that must read exactly as it runs: an
/// approval's summary and a request's whole input, where `a   b` must not read `a b`. Hard
/// line breaks start rows of their own, as in [`wrap_joined`].
fn wrap_exact(line: Line<'static>, width: usize) -> Vec<(Line<'static>, Join)> {
    if has_breaks(&line) {
        return hard_lines(line)
            .into_iter()
            .flat_map(|l| wrap_exact(l, width))
            .collect();
    }
    let width = width.max(1);
    let style = line.style;
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut ends = Vec::with_capacity(line.spans.len());
    let mut end = 0;
    for s in &line.spans {
        end += s.content.len();
        ends.push((end, s.style));
    }
    let mut rows: Vec<(Vec<Span<'static>>, Join)> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut join = Join::Newline;
    let mut used = 0;
    for (at, g) in clusters(&text) {
        let k = ends.partition_point(|&(end, _)| end <= at);
        let st = ends.get(k).map_or(Style::default(), |&(_, st)| st);
        let gw = cluster_width(g);
        if used > 0 && used + gw > width {
            rows.push((std::mem::take(&mut row), join));
            join = Join::Glue;
            used = 0;
        }
        match row.last_mut() {
            Some(last) if last.style == st => last.content.to_mut().push_str(g),
            _ => row.push(Span::styled(g.to_string(), st)),
        }
        used += gw;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push((row, join));
    }
    rows.into_iter()
        .map(|(spans, join)| (Line::from(spans).style(style), join))
        .collect()
}

/// Whether `line` holds a hard line break (`\n` or `\r`), which the terminal would not draw.
fn has_breaks(line: &Line<'_>) -> bool {
    line.spans.iter().any(|s| s.content.contains(['\n', '\r']))
}

/// `line` split at its hard line breaks (`\n`; a `\r` is dropped): one line per part, each
/// span keeping its style and each line the style of `line`.
fn hard_lines(line: Line<'static>) -> Vec<Line<'static>> {
    let mut parts: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for span in &line.spans {
        for (i, part) in span.content.split('\n').enumerate() {
            if i > 0 {
                parts.push(Vec::new());
            }
            let part = part.replace('\r', "");
            if !part.is_empty() {
                if let Some(row) = parts.last_mut() {
                    row.push(Span::styled(part, span.style));
                }
            }
        }
    }
    parts
        .into_iter()
        .map(|spans| Line::from(spans).style(line.style))
        .collect()
}

/// Base style of a thread's card or tile: dimmed while parked.
fn parked_style(t: &ThreadView) -> Style {
    if t.parked {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    }
}

/// `branch · changes made · ↑ahead ↓behind` while the worktree has uncommitted changes, else
/// `branch · ↑ahead ↓behind` (`(detached)` without a branch). Against its base when it has one
/// (merge readiness): `twr/api · ↑4 ↓12 dev`, then [`conflicts_text`]; else its upstream.
pub fn git_text(g: &GitSnapshot) -> String {
    let branch = g.branch.as_deref().unwrap_or("(detached)");
    let changes = if g.dirty_files > 0 {
        " · changes made"
    } else {
        ""
    };
    match (&g.base, g.base_ahead, g.base_behind) {
        (Some(base), Some(ahead), Some(behind)) => {
            let base = short_ref(base);
            format!("{branch}{changes} · ↑{ahead} ↓{behind} {base}")
        }
        _ => format!("{branch}{changes} · ↑{} ↓{}", g.ahead, g.behind),
    }
}

/// A ref as a card shows it: `dev` for `refs/heads/dev`, `origin/main` for a remote branch,
/// `tags/v1` for a tag, a commit's first 10 hex digits.
pub fn short_ref(r: &str) -> String {
    if let Some(b) = r.strip_prefix("refs/heads/") {
        return b.to_string();
    }
    if let Some(b) = r.strip_prefix("refs/remotes/") {
        return b.to_string();
    }
    if let Some(t) = r.strip_prefix("refs/") {
        return t.to_string();
    }
    if r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()) {
        return r[..10].to_string();
    }
    r.to_string()
}

/// `conflicts: 2 files · ` when a merge with its base would conflict (it leads the git line,
/// so a narrow card still shows it); empty otherwise.
pub fn conflicts_text(g: &GitSnapshot) -> String {
    let n = g
        .conflict_count
        .or(g.conflicts.as_ref().map(|c| c.len() as u64))
        .unwrap_or(0);
    match n {
        // A conflict git names no file for (a directory rename).
        0 if g.conflicted == Some(true) => "conflicts · ".into(),
        0 => String::new(),
        1 => "conflicts: 1 file · ".into(),
        n => format!("conflicts: {n} files · "),
    }
}

fn git_line(g: &GitSnapshot) -> Line<'static> {
    let color = if g.dirty_files > 0 {
        Color::Yellow
    } else {
        Color::Cyan
    };
    let mut spans = Vec::new();
    let conflicts = conflicts_text(g);
    if !conflicts.is_empty() {
        spans.push(Span::styled(conflicts, Style::default().fg(Color::Red)));
    }
    spans.push(Span::styled(git_text(g), Style::default().fg(color)));
    Line::from(spans)
}

/// `runtime@box` (just the runtime for the default box, `?` when unknown).
fn runtime_box(t: &ThreadView) -> String {
    let runtime = if t.runtime.is_empty() {
        "?"
    } else {
        t.runtime.as_str()
    };
    if t.box_name.is_empty() {
        runtime.to_string()
    } else {
        format!("{runtime}@{}", t.box_name)
    }
}

/// A model id short enough for a card or tile: a trailing date (`-20250929`, `-2025-09-29`)
/// and a leading `claude-` before the family name are dropped, so `claude-sonnet-4-5-20250929`
/// reads `sonnet-4-5` (next to the `claude-code` runtime). The full id is in the transcript
/// header.
pub fn short_model(id: &str) -> String {
    let id = id.trim();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mut s = id;
    if let Some((head, date)) = s.rsplit_once('-') {
        if date.len() == 8 && digits(date) && !head.is_empty() {
            s = head;
        }
    }
    if s.len() > 11 && s.is_char_boundary(s.len() - 11) {
        let (head, date) = s.split_at(s.len() - 11);
        let parts: Vec<&str> = date.split('-').collect();
        if !head.is_empty()
            && matches!(parts.as_slice(), [d0, y, m, d]
                if d0.is_empty() && y.len() == 4 && m.len() == 2 && d.len() == 2
                    && digits(y) && digits(m) && digits(d))
        {
            s = head;
        }
    }
    // `claude-3-5-haiku` keeps its prefix: `3-5-haiku` would not read as a model.
    match s.strip_prefix("claude-") {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_alphabetic()) => rest.to_string(),
        _ => s.to_string(),
    }
}

/// ` · high`: the thread's reasoning effort as it follows the model, or nothing when it has
/// none (the runtime's default).
fn effort_suffix(t: &ThreadView) -> String {
    t.effort
        .as_deref()
        .map(|e| format!(" · {e}"))
        .unwrap_or_default()
}

/// The model a card or tile shows: the runtime's reported one, else the requested one
/// (shortened, [`short_model`]), else `default model` (the runtime's own default, not reported
/// yet or never); then the reasoning effort when the thread has one (`fable-5-1 · high`).
pub fn model_label(t: &ThreadView) -> String {
    let model = t
        .shown_model()
        .map(short_model)
        .unwrap_or_else(|| "default model".into());
    format!("{model}{}", effort_suffix(t))
}

/// The full model of the transcript header: the id the runtime reports, else the requested
/// one marked `(requested)`, else `default model`; then the reasoning effort, as on a card.
pub fn model_full(t: &ThreadView) -> String {
    let model = match (&t.actual_model, &t.model) {
        (Some(m), _) => m.clone(),
        (None, Some(m)) => format!("{m} (requested)"),
        (None, None) => "default model".into(),
    };
    format!("{model}{}", effort_suffix(t))
}

/// The header of a card or tile: `status · runtime · model` on one line when it fits in
/// `width`, else the status, then `runtime · model` (cut with `…`). The working icon, if
/// any, comes first; it never adds a row: a line that fits without it is cut at its end
/// (the model gives way) to make room for it.
fn status_lines(
    t: &ThreadView,
    runtime: String,
    width: usize,
    working: Option<Span<'static>>,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let status = t.status();
    let status_text = card_status(t);
    let mut lead: Vec<Span<'static>> = match working {
        Some(cursor) => vec![cursor, Span::raw(" ")],
        None => Vec::new(),
    };
    let room = width.saturating_sub(Line::from(lead.clone()).width());
    let status_span = Span::styled(
        status_text.clone(),
        Style::default()
            .fg(status_color(status))
            .add_modifier(Modifier::BOLD),
    );
    let model = model_label(t);
    let one = crate::edit::width(&status_text)
        + 3
        + crate::edit::width(&runtime)
        + 3
        + crate::edit::width(&model);
    if one <= width {
        lead.extend(cut_spans(
            vec![
                status_span,
                Span::styled(" · ", dim),
                Span::raw(runtime),
                Span::styled(" · ", dim),
                Span::raw(model),
            ],
            room,
        ));
        return vec![Line::from(lead)];
    }
    lead.push(status_span);
    vec![
        Line::from(lead),
        Line::from(Span::raw(truncate_cells(
            &format!("{runtime} · {model}"),
            width,
        ))),
    ]
}

/// `spans` cut to `width` characters, the last one shown being `…` when they are cut.
fn cut_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| crate::edit::width(&s.content)).sum();
    if total <= width {
        return spans;
    }
    let mut left = width.saturating_sub(1);
    let mut out = Vec::new();
    for s in spans {
        let n = crate::edit::width(&s.content);
        if n <= left {
            left -= n;
            out.push(s);
            continue;
        }
        // Whole clusters only, within the cells left.
        let mut head = String::new();
        for (_, g) in clusters(&s.content) {
            let w = cluster_width(g);
            if w > left {
                break;
            }
            left -= w;
            head.push_str(g);
        }
        out.push(Span::styled(head, s.style));
        break;
    }
    if width > 0 {
        out.push(Span::raw("…"));
    }
    out
}

/// The badge span for an attention reason.
fn badge(reason: AttentionReason) -> Span<'static> {
    Span::styled(
        format!(" {} ", reason_label(reason)),
        Style::default()
            .fg(Color::Black)
            .bg(reason_color(reason))
            .add_modifier(Modifier::BOLD),
    )
}

/// The top attention item of a card `width` cells wide inside, in rows ([`wrap_line`]).
struct AttnRows {
    /// The item: id and revision.
    item: (String, u64),
    /// Its badge and summary, then every option it lists ([`option_rows`]): what must be on
    /// screen for an option key to answer it.
    core: Vec<Line<'static>>,
    /// Its note and how many more items wait (cards and banners; a tile says `(+N)` after the
    /// summary).
    extra: Vec<Line<'static>>,
    /// Every row of `core` fits the width (false only for a character wider than a row).
    exact: bool,
    /// The revision's full text is known: not only its capped preview
    /// ([`crate::app::Incomplete`]), which is drawn marked as cut short and never counts as
    /// drawn in full.
    complete: bool,
}

/// A confirm card's detail view in rows ([`card_detail_rows`]).
struct DetailRows {
    /// Every row: its badge and the head of its summary, each action, a blank row, its
    /// options, its note and how many more items wait.
    rows: Vec<Line<'static>>,
    /// How each row continues the row above ([`Join`]), for a request drawn in the
    /// conversation ([`request_rows`]); empty for a confirm card's.
    joins: Vec<Join>,
    /// The range of its option rows.
    options: std::ops::Range<usize>,
    /// Every row fits the width (false only for a character wider than a row).
    exact: bool,
}

/// Attention items in rows kept from earlier frames ([`attention_rows`] for cards, tiles and
/// banners, [`card_detail_rows`] for a confirm card's detail view), so a frame wraps an
/// item's text only when it changed: wrapping a summary of a megabyte takes far longer than a
/// frame, and the working icon draws 8 frames a second. Rows are kept by the item's id and
/// revision, the width and what else they show ([`WrapKey`]), and used only while the item's
/// text is still the text they were wrapped from (the full text of a capped preview and a
/// note come in the same revision). What the last frame did not use is dropped when the next
/// one starts ([`render`]). Not part of the state: a clone starts empty.
#[derive(Default)]
pub struct WrapCache(Mutex<Wraps>);

/// The rows [`WrapCache`] keeps.
#[derive(Default)]
struct Wraps {
    /// The frame being drawn.
    frame: u64,
    /// Cards, tiles and banners.
    attn: HashMap<WrapKey, Wrapped<AttnRows>>,
    /// Confirm cards' detail views.
    detail: HashMap<WrapKey, Wrapped<DetailRows>>,
    /// Worker permission/question rows, shared by inline cards and their detail view.
    requests: HashMap<WrapKey, Wrapped<DetailRows>>,
    /// How many times an item was wrapped (its rows were not kept).
    misses: u64,
}

/// What an item's rows are kept by ([`WrapCache`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WrapKey {
    attn_id: String,
    revision: u64,
    /// The wrap width.
    width: usize,
    /// A grid tile's rows.
    tile: bool,
    /// How many more items wait after it.
    more: usize,
    /// Its text is only a capped preview: loading (`Some(false)`) or not loadable
    /// (`Some(true)`).
    cut: Option<bool>,
}

/// An item's rows ([`WrapCache`]): the text they were wrapped from (its badge too: an item
/// folded without a position keeps revision 0 when raised again) and the last frame that used
/// them.
struct Wrapped<V> {
    reason: AttentionReason,
    summary: String,
    options: Vec<String>,
    note: Option<String>,
    context: Option<RequestContext>,
    rows: Arc<V>,
    used: u64,
}

impl<V> Wrapped<V> {
    /// Whether these rows show `item`'s text.
    fn shows(&self, item: &AttentionItem) -> bool {
        self.reason == item.reason
            && self.summary == item.summary
            && self.options == item.options
            && self.note == item.note
            && self.context == item.context
    }
}

impl WrapCache {
    fn lock(&self) -> MutexGuard<'_, Wraps> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a frame: drops the rows the last frame did not use.
    pub fn next_frame(&self) {
        let mut w = self.lock();
        let last = w.frame;
        w.frame += 1;
        w.attn.retain(|_, e| e.used >= last);
        w.detail.retain(|_, e| e.used >= last);
        w.requests.retain(|_, e| e.used >= last);
    }

    /// How many times an item was wrapped so far (each time the rows asked for were not
    /// kept).
    pub fn misses(&self) -> u64 {
        self.lock().misses
    }

    /// How many items' rows are kept.
    pub fn kept(&self) -> usize {
        let w = self.lock();
        w.attn.len() + w.detail.len() + w.requests.len()
    }

    /// `item`'s card, tile or banner rows for `key`, made by `make` unless kept.
    fn attn(
        &self,
        key: WrapKey,
        item: &AttentionItem,
        make: impl FnOnce() -> AttnRows,
    ) -> Arc<AttnRows> {
        let mut guard = self.lock();
        let w = &mut *guard;
        kept(&mut w.attn, w.frame, &mut w.misses, key, item, make)
    }

    /// `item`'s detail view rows for `key`, made by `make` unless kept.
    fn detail(
        &self,
        key: WrapKey,
        item: &AttentionItem,
        make: impl FnOnce() -> DetailRows,
    ) -> Arc<DetailRows> {
        let mut guard = self.lock();
        let w = &mut *guard;
        kept(&mut w.detail, w.frame, &mut w.misses, key, item, make)
    }

    fn request(
        &self,
        key: WrapKey,
        item: &AttentionItem,
        make: impl FnOnce() -> DetailRows,
    ) -> Arc<DetailRows> {
        let mut guard = self.lock();
        let w = &mut *guard;
        kept(&mut w.requests, w.frame, &mut w.misses, key, item, make)
    }
}

/// The rows in `map` for `key` if they show `item`'s text, else `make`'s (kept for the next
/// frame); either is marked used in `frame`.
fn kept<V>(
    map: &mut HashMap<WrapKey, Wrapped<V>>,
    frame: u64,
    misses: &mut u64,
    key: WrapKey,
    item: &AttentionItem,
    make: impl FnOnce() -> V,
) -> Arc<V> {
    if let Some(e) = map.get_mut(&key).filter(|e| e.shows(item)) {
        e.used = frame;
        return Arc::clone(&e.rows);
    }
    *misses += 1;
    let rows = Arc::new(make());
    map.insert(
        key,
        Wrapped {
            reason: item.reason,
            summary: item.summary.clone(),
            options: item.options.clone(),
            note: item.note.clone(),
            context: item.context.clone(),
            rows: Arc::clone(&rows),
            used: frame,
        },
    );
    rows
}

impl Clone for WrapCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for WrapCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let w = self.lock();
        f.debug_struct("WrapCache")
            .field("frame", &w.frame)
            .field("attn", &w.attn.len())
            .field("detail", &w.detail.len())
            .field("misses", &w.misses)
            .finish()
    }
}

/// `thread`'s top attention item as rows `width` cells wide ([`AttnRows`]); `tile`: a grid
/// tile's (no note, `(+N)` after the summary). `None` when nothing is open. Wrapped once and
/// kept while the item, its text and the width stay the same ([`WrapCache`]).
fn attention_rows(
    state: &AppState,
    thread: &str,
    width: usize,
    tile: bool,
) -> Option<Arc<AttnRows>> {
    let items = state.card_items(thread);
    let top = *items.first()?;
    let incomplete = state.incomplete(top);
    let key = WrapKey {
        attn_id: top.attn_id.clone(),
        revision: top.revision,
        width,
        tile,
        more: items.len() - 1,
        cut: incomplete.map(|i| i.failed),
    };
    let (more, cut) = (key.more, key.cut);
    let top = shown_item(state, top);
    let top = top.as_ref();
    Some(
        state
            .wraps
            .attn(key, top, || attention_rows_of(top, more, cut, width, tile)),
    )
}

/// `item` as the views show it: a delete card, whose text is the hub's own, with the commands
/// it quotes (`` `twr trash restore` ``) run in the view's workspace
/// ([`AppState::attach_via`]). Anything else is shown as raised: a runtime's request, or a
/// confirm card quoting one, may quote a command it would run, which approving runs as
/// written. The item itself, what an answer refers to, stays as raised.
fn shown_item<'a>(
    state: &AppState,
    item: &'a AttentionItem,
) -> std::borrow::Cow<'a, AttentionItem> {
    if item.thread != HUB_THREAD || state.delete_card(&item.attn_id).is_none() {
        return std::borrow::Cow::Borrowed(item);
    }
    let summary = twr_core::paths::localize_commands(&item.summary, &state.attach_via);
    if summary == item.summary {
        std::borrow::Cow::Borrowed(item)
    } else {
        let mut shown = item.clone();
        shown.summary = summary;
        std::borrow::Cow::Owned(shown)
    }
}

/// [`attention_rows`] for `top`, with `more` items waiting after it; `cut`: its text is only a
/// capped preview, whose full text is loading (`Some(false)`) or could not be loaded
/// (`Some(true)`).
fn attention_rows_of(
    top: &AttentionItem,
    more: usize,
    cut: Option<bool>,
    width: usize,
    tile: bool,
) -> AttnRows {
    let dim = Style::default().fg(Color::DarkGray);
    let mut head = vec![
        badge(top.reason),
        Span::raw(" "),
        Span::raw(top.summary.clone()),
    ];
    if let Some(failed) = cut {
        let why = if failed {
            " (could not load the full text)"
        } else {
            " (truncated: loading…)"
        };
        head.push(Span::styled(
            why,
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    if tile && more > 0 {
        head.push(Span::styled(format!(" (+{more})"), dim));
    }
    let mut core = wrap_line(Line::from(head), width);
    core.extend(option_rows(&top.options, width));
    let mut extra = Vec::new();
    if !tile {
        // Why the agent asks, when its runtime says, on one row (the card's status and
        // activity keep theirs): the request's own detail ([`request_detail_rows`]) has all
        // of it and the rest of its context.
        if let Some(why) = top.context.as_ref().and_then(|c| c.why.as_deref()) {
            let row = truncate_cells(&format!("why: {}", one_row(why)), width);
            extra.push(Line::from(Span::styled(
                row,
                Style::default().fg(Color::DarkGray),
            )));
        }
        if let Some(note) = &top.note {
            extra.extend(wrap_line(
                Line::from(Span::styled(
                    format!("note: {note}"),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::ITALIC),
                )),
                width,
            ));
        }
        if more > 0 {
            extra.extend(wrap_line(
                Line::from(Span::styled(format!("+{more} more"), dim)),
                width,
            ));
        }
    }
    let exact = core.iter().all(|l| l.width() <= width.max(1));
    AttnRows {
        item: (top.attn_id.clone(), top.revision),
        core,
        extra,
        exact,
        complete: cut.is_none(),
    }
}

/// A request's context ([`RequestContext`]) in rows `width` cells wide: each
/// field after its dim label (`why: ...`), wrapped.
fn context_rows(fields: &[(&str, &str)], width: usize) -> Vec<(Line<'static>, Join)> {
    let label = Style::default().fg(Color::DarkGray);
    let text = Style::default().fg(Color::Gray);
    fields
        .iter()
        .flat_map(|(k, v)| {
            let line = Line::from(vec![
                Span::styled(format!("{k}: "), label),
                Span::styled(v.to_string(), text),
            ]);
            // A request's whole input reads exactly as it runs ([`wrap_exact`]).
            if *k == "input" {
                wrap_exact(line, width)
            } else {
                wrap_joined(line, width)
            }
        })
        .collect()
}

/// Every option, numbered: `1 allow  2 deny` for the first nine (their number keys answer
/// them), then `10. label` (no key answers those, but they are part of what the item asks: an
/// item is drawn in full only with all of them on screen), in rows of at most `width` cells:
/// an option that does not fit on a row starts the next one; one wider than a row, or holding a
/// line break, is wrapped on rows of its own.
fn option_rows(options: &[String], width: usize) -> Vec<Line<'static>> {
    option_rows_joined(options, width)
        .into_iter()
        .map(|(l, _)| l)
        .collect()
}

/// [`option_rows`], each row with how it continues the row above ([`Join`]): the options are
/// one line, however they are packed into rows, so a place in them is the same text at any
/// width (an option wrapped inside a word continues right after it).
fn option_rows_joined(options: &[String], width: usize) -> Vec<(Line<'static>, Join)> {
    let key = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let no_key = Style::default().fg(Color::DarkGray);
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (n, opt) in options.iter().enumerate() {
        let number = if n < 9 {
            Span::styled(format!("{}", n + 1), key)
        } else {
            Span::styled(format!("{}.", n + 1), no_key)
        };
        let chunk = Line::from(vec![number, Span::raw(format!(" {opt}"))]);
        let w = chunk.width();
        // An option with a hard line break gets rows of its own, like one wider than a row.
        let fits = w <= width && !has_breaks(&chunk);
        if fits && !row.is_empty() && used + 2 + w <= width {
            row.push(Span::raw("  "));
            row.extend(chunk.spans);
            used += 2 + w;
            continue;
        }
        if !row.is_empty() {
            rows.push((Line::from(std::mem::take(&mut row)), Join::Newline));
            used = 0;
        }
        if fits {
            row = chunk.spans;
            used = w;
        } else {
            rows.extend(wrap_joined(chunk, width));
        }
    }
    if !row.is_empty() {
        rows.push((Line::from(row), Join::Newline));
    }
    for (k, (_, join)) in rows.iter_mut().enumerate() {
        if k > 0 && *join != Join::Glue {
            *join = Join::Space;
        }
    }
    rows
}

/// The hub's card `width` cells wide inside: `routing`, then its top question
/// ([`attention_rows`]).
fn hub_card_parts(state: &AppState, width: usize) -> (Vec<Line<'static>>, Option<Arc<AttnRows>>) {
    let body = vec![Line::from(Span::styled(
        "routing",
        Style::default().fg(Color::DarkGray),
    ))];
    (body, attention_rows(state, HUB_THREAD, width, false))
}

/// The hub's card: its open questions (which thread should get a message), answered from its
/// action popup like any card. Returns the item it showed in full (id and revision).
fn render_hub_card(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    look: Look,
    (more_left, more_right): (Option<String>, Option<String>),
) -> Option<(String, u64)> {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            " twr asks ",
            Style::default().add_modifier(Modifier::BOLD),
        )));
    let mut block = card_border(block, look, Style::default().fg(Color::Yellow));
    if let Some(l) = more_left {
        block = block.title_bottom(Line::from(l).left_aligned());
    }
    if let Some(r) = more_right {
        block = block.title_bottom(Line::from(r).right_aligned());
    }
    let inner = block.inner(area);
    let (body, attn) = hub_card_parts(state, usize::from(inner.width));
    let (rows, drawn) = card_rows(body, attn, inner.height, inner.width);
    frame.render_widget(Paragraph::new(rows).block(block), area);
    drawn
}

fn render_grid_view(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let [grid, input, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(input_height(state, area)),
        Constraint::Length(status_height(state, area)),
    ])
    .areas(area);
    let noted = render_grid(frame, grid, state, panes);
    render_input(frame, input, state, panes);
    let parked = parked_note(state).filter(|_| !noted);
    render_status(frame, status, state, panes, parked);
}

/// First visible grid row so that `focus_row` shows when `fit` rows fit.
pub fn grid_first_row(focus_row: usize, fit: usize) -> usize {
    (focus_row + 1).saturating_sub(fit.max(1))
}

/// The grid of tiles under a header; returns whether the hidden parked threads are noted (the
/// header's `(N hidden, /parked shows)`, or the empty pane's [`parked_note`]).
fn render_grid(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) -> bool {
    let order = state.grid_order();
    let [header, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    if order.is_empty() {
        // No tile: still the way back.
        let head = Line::from(vec![
            Span::styled(" ‹ stack", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(" · grid", Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(head), header);
        panes.add(PaneId::Text, header);
        if let Some(cells) = find_text(frame.buffer_mut(), header, " ‹ stack ·") {
            panes.target(cells, Target::Stack);
        }
        return render_empty(frame, body, state, panes);
    }
    let cols = grid_columns(area.width);
    let rows = order.len().div_ceil(cols);
    let fit = usize::from(body.height / TILE_HEIGHT).max(1);
    let focus_idx = state
        .focused()
        .and_then(|f| order.iter().position(|id| id == f));
    let max = rows.saturating_sub(fit);
    let layout = ListLayout::Grid(cols);
    let focus_row = focus_idx.map(|i| i / cols);
    let fresh = grid_first_row(focus_row.unwrap_or(0), fit);
    let first = list_start(state, layout, &order, cols, focus_row, fresh, max, |_| fit);
    panes.list_first = order.get(first * cols).cloned();
    panes.list = Some(ListWindow {
        layout,
        area: body,
        start: first,
        max,
    });
    let last = (first + fit).min(rows);

    let threads = order.iter().filter(|id| *id != HUB_THREAD).count();
    let parked = order
        .iter()
        .filter(|id| state.registry.get(id).is_some_and(|t| t.parked))
        .count();
    let dim = Style::default().fg(Color::DarkGray);
    let mut head = vec![
        Span::styled(" ‹ stack", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(" ·", dim),
        Span::styled(
            format!(
                " grid · {threads} thread{}",
                if threads == 1 { "" } else { "s" }
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(name) = state.instance_label() {
        head.insert(
            0,
            Span::styled(
                format!(" [{}]", truncate_cells(name, usize::from(header.width) / 3)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        );
    }
    let hidden = state.hidden_parked().len();
    if parked + hidden > 0 {
        let note = match (parked, hidden) {
            (_, 0) => String::new(),
            (0, _) => " (hidden, /parked shows)".to_string(),
            (_, h) => format!(" ({h} hidden, /parked shows)"),
        };
        head.push(Span::styled(
            format!(" · {} parked{note}", parked + hidden),
            dim,
        ));
    }
    // Noted when the header shows its `hidden` part whole.
    let noted = hidden > 0 && Line::from(head.clone()).width() <= usize::from(header.width);
    if first > 0 {
        head.push(Span::styled(format!(" · ↑ {first} more rows"), dim));
    }
    if last < rows {
        head.push(Span::styled(format!(" · ↓ {} more rows", rows - last), dim));
    }
    if let Some(item) = state
        .orchestrator()
        .and_then(|o| state.top_attention(&o.id))
    {
        head.push(Span::raw(" · "));
        head.push(badge(item.reason));
        head.push(Span::styled(
            " the orchestrator needs you (Ctrl-G: back to the conversation)",
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(head)), header);
    panes.add(PaneId::Text, header);
    // `‹ stack` back to the stack view (Ctrl-G), the parked note lists them (`/parked`).
    if let Some(cells) = find_text(frame.buffer_mut(), header, " ‹ stack ·") {
        panes.target(cells, Target::Stack);
    }
    if let Some(cells) = find_text(frame.buffer_mut(), header, "/parked shows") {
        panes.target(cells, Target::Parked);
    }

    let slots = Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols]).split(body);
    for row in first..last {
        let y = body.y + (row - first) as u16 * TILE_HEIGHT;
        let height = TILE_HEIGHT.min(body.bottom().saturating_sub(y));
        if height == 0 {
            break;
        }
        for (col, slot) in slots.iter().enumerate() {
            let i = row * cols + col;
            let Some(id) = order.get(i) else {
                break;
            };
            let tile = Rect::new(slot.x, y, slot.width, height);
            let look = look(state, id);
            let inner = Block::default().borders(Borders::ALL).inner(tile);
            panes.add(PaneId::Thread(id.clone()), inner);
            let drawn = if id == HUB_THREAD {
                render_hub_card(frame, tile, state, look, (None, None))
            } else if let Some(t) = state.registry.get(id) {
                render_tile(frame, tile, state, t, look, panes)
            } else {
                None
            };
            record(panes, id, drawn, inner);
        }
    }
    noted
}

/// One grid tile: status (with its report's grade, [`card_status`]), runtime/box, git and
/// turns, then the top attention item (badge, summary and options, kept whole first) or else
/// the slot a card has ([`slot_lines`]): what it is doing, or its last report, cut with `…`.
/// Its project is in its title ([`card_block`]). Returns the item it showed in full (id and
/// revision). Its pane is recorded already.
fn render_tile(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    t: &ThreadView,
    look: Look,
    panes: &mut Panes,
) -> Option<(String, u64)> {
    let dim = Style::default().fg(Color::DarkGray);
    let block = card_block(t, state.view.pins.contains(&t.id), area.width);
    let block = card_border(block, look, dim);
    let inner_width = usize::from(area.width.saturating_sub(2));

    let turns = match t.turns {
        1 => "1 turn".to_string(),
        n => format!("{n} turns"),
    };
    let mut lines = status_lines(t, runtime_box(t), inner_width, working_icon(state, t));
    // The turn count (and what the turns spent) after the git state, which is cut (`…`) to
    // leave it room.
    let turns = match t.spent.short() {
        Some(spent) => format!(" · {turns} · {spent}"),
        None => format!(" · {turns}"),
    };
    let git = match &t.git {
        Some(g) => git_line(g),
        None => Line::from(Span::styled("no git snapshot", dim)),
    };
    let room = inner_width.saturating_sub(crate::edit::width(&turns));
    let mut spans = cut_spans(git.spans, room);
    spans.push(Span::styled(turns, dim));
    lines.push(Line::from(spans));
    let inner = block.inner(area);
    let status_row = lines.first().filter(|_| icon::is_working(t)).cloned();
    if let Some(attn) = attention_rows(state, &t.id, usize::from(inner.width), true) {
        let (rows, drawn) = card_rows(lines, Some(attn), inner.height, inner.width);
        if let Some(cells) = shows_icon(&rows, status_row.as_ref()) {
            note_icon(panes, state, &t.id, inner, cells);
        }
        frame.render_widget(
            Paragraph::new(rows).block(block).style(parked_style(t)),
            area,
        );
        return drawn;
    }
    // Then the slot, as on a card: what it does or what it came to, cut to the tile.
    lines.extend(slot_lines(state, t, inner_width));
    let (rows, _) = card_rows(lines, None, inner.height, inner.width);
    // Not when the tile's one row is cut (`…` in its place).
    if let Some(cells) = shows_icon(&rows, status_row.as_ref()) {
        note_icon(panes, state, &t.id, inner, cells);
    }
    frame.render_widget(
        Paragraph::new(rows).block(block).style(parked_style(t)),
        area,
    );
    None
}

fn status_color(s: ThreadStatus) -> Color {
    match s {
        ThreadStatus::Running | ThreadStatus::Background => Color::Green,
        ThreadStatus::Waiting => Color::Yellow,
        ThreadStatus::Error => Color::Red,
        ThreadStatus::Done => Color::Blue,
        ThreadStatus::Idle => Color::Gray,
        ThreadStatus::Parked => Color::DarkGray,
        ThreadStatus::Attached => Color::Magenta,
    }
}

fn reason_color(r: AttentionReason) -> Color {
    match r {
        AttentionReason::Permission => Color::Red,
        AttentionReason::Question => Color::Yellow,
        AttentionReason::Error => Color::LightRed,
        AttentionReason::Review => Color::Cyan,
        AttentionReason::Stalled => Color::Gray,
    }
}

/// Transcript entries as styled, pre-wrapped lines for a pane `width` columns wide.
pub fn transcript_lines(entries: &[Entry], width: u16) -> Vec<Line<'static>> {
    transcript_rows(entries, width)
        .into_iter()
        .map(|(l, _)| l)
        .collect()
}

/// [`transcript_lines`], each with how it continues the line above ([`Join`]): a copied
/// paragraph is one line again.
pub fn transcript_rows(entries: &[Entry], width: u16) -> Vec<(Line<'static>, Join)> {
    transcript_view(entries, width, &Collapse::default(), &Threads::default())
        .into_iter()
        .map(|(l, j, _, _)| (l, j))
        .collect()
}

/// What the main pane folds away: tool calls (the orchestrator's twr actions, a worker's tool
/// uses in a dive) and the hub's notice turns, each group as one dim line, unless expanded;
/// in the orchestrator conversation also the line that ends a completed turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct Collapse<'a> {
    /// Fold (the main pane: the orchestrator conversation and dives alike).
    pub on: bool,
    /// Leave out the `── completed ──` line of a turn that completed (the orchestrator
    /// conversation: its replies follow one another). A turn that failed or was interrupted
    /// still ends with its line; a dive keeps every one.
    pub quiet_ends: bool,
    /// Ctrl-O: everything expanded.
    pub all: bool,
    /// Groups expanded one by one (by a click on their line).
    pub expanded: Option<&'a HashSet<String>>,
}

impl Collapse<'_> {
    fn folded(&self, key: &str) -> bool {
        self.on && !self.all && !self.expanded.is_some_and(|e| e.contains(key))
    }
}

/// The line a folded hub notice turn shows.
pub const NOTICE_MARKER: &str = "· hub update";

/// A transcript row as the main pane draws it: the line, how it continues the row above
/// ([`Join`]), the folded group a click on it expands (only a group's marker row has one),
/// and which of its cells are a thread box's frame ([`Edge`]).
pub type TranscriptRow = (Line<'static>, Join, Option<String>, Edge);

/// Which cells of a transcript row are a thread box's frame ([`thread_box`]): drawn, but left
/// out of copied text, so a copied box is its title and its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Edge {
    /// No frame: every cell is text.
    #[default]
    None,
    /// A box's top border with a title this many cells wide after its first three cells
    /// (`╭─ `): the cells around the title.
    Top(usize),
    /// A box's text row: its first two cells (`│ `) and its last two (` │`).
    Bars,
    /// A box's bottom border: the whole row (and its [`Join::Skip`]: not even a line break is
    /// copied for it).
    Bottom,
    /// A row's first this many cells are indentation or a marker (`● `, `❯ `, a list item's
    /// hanging indent): drawn, left out of a copy.
    Gutter(u16),
    /// A box's text row whose text is indented this many cells (a list item's hanging
    /// indent): its frame, as [`Edge::Bars`], and that indentation.
    BarsIndent(u16),
}

/// The cells of a row at `(x, y)`, `width` cells wide, that are frame by `edge` ([`Edge`]).
pub fn frame_cells(edge: Edge, x: u16, y: u16, width: u16) -> Vec<Rect> {
    let cells = |from: u16, w: u16| {
        let w = w.min(width.saturating_sub(from));
        (w > 0).then(|| Rect::new(x.saturating_add(from), y, w, 1))
    };
    match edge {
        Edge::None => Vec::new(),
        Edge::Top(title) => {
            let after = u16::try_from(title).unwrap_or(u16::MAX).saturating_add(3);
            [cells(0, 3), cells(after, width)]
                .into_iter()
                .flatten()
                .collect()
        }
        Edge::Bars => [cells(0, 2), cells(width.saturating_sub(2).max(2), 2)]
            .into_iter()
            .flatten()
            .collect(),
        Edge::Bottom => cells(0, width).into_iter().collect(),
        Edge::Gutter(n) => cells(0, n).into_iter().collect(),
        Edge::BarsIndent(n) => [
            cells(0, n.saturating_add(2)),
            cells(width.saturating_sub(2).max(2), 2),
        ]
        .into_iter()
        .flatten()
        .collect(),
    }
}

/// What the orchestrator conversation knows of the worker threads it talks about
/// ([`transcript_view`]): whether it draws the orchestrator's thread sections as boxes, and
/// the state thread names are looked up in, for their status and color.
#[derive(Debug, Clone, Copy, Default)]
pub struct Threads<'a> {
    /// Draw each `<thread name="…">` … `</thread>` section of the text as a box
    /// ([`thread_sections`]): the orchestrator's conversation. A worker's transcript shows its
    /// text as it is.
    pub boxes: bool,
    /// Where thread names are looked up; `None` knows no thread (every box is neutral, and no
    /// folded line is named).
    pub state: Option<&'a AppState>,
}

impl<'a> Threads<'a> {
    /// The thread `name` names exactly: its name, id or an alias, or a name it had before it
    /// was renamed (a box written before the rename). No prefix match: a box names one thread
    /// or none.
    fn get(&self, name: &str) -> Option<&'a ThreadView> {
        let t = self.state?.registry.resolve(name).ok()?;
        (t.name == name
            || t.id == name
            || t.aliases.iter().any(|a| a == name)
            || t.former_names.iter().any(|n| n == name))
        .then_some(t)
    }
}

/// The colors of thread boxes and of thread names on folded lines: fixed 256-color values (a
/// blue, a green, a violet, a tan, a mauve and a grey teal), none of them the orchestrator's
/// cyan (80), a dive's magenta (176), the working cursor's orange (173) or a confirm card's
/// yellow.
pub const THREAD_COLORS: [Color; 6] = [
    Color::Indexed(75),
    Color::Indexed(114),
    Color::Indexed(141),
    Color::Indexed(180),
    Color::Indexed(139),
    Color::Indexed(109),
];

/// Thread `id`'s color, one of [`THREAD_COLORS`]: the same for the same id on every frame
/// and every run (FNV-1a of its bytes).
pub fn thread_color(id: &str) -> Color {
    let mut h: u32 = 0x811c_9dc5;
    for b in id.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    THREAD_COLORS[h as usize % THREAD_COLORS.len()]
}

/// A part of the orchestrator's reply ([`thread_sections`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Section {
    /// Text outside any thread section, as written.
    Plain(String),
    /// A thread section's text, without its tags or the blank lines it starts and ends with.
    Thread {
        /// The name in its opening tag.
        name: String,
        /// Its text.
        text: String,
        /// Its end came: its `</thread>` line, the next opening tag or the end of the
        /// finished reply. A section still streaming in is open.
        closed: bool,
    },
}

/// How a thread section's opening tag starts.
const OPEN_TAG: &str = "<thread name=\"";

/// The name in a `<thread name="NAME">` line: the tag alone on its line (spaces around it
/// allowed), with a name that is not empty.
fn open_tag(line: &str) -> Option<&str> {
    let name = line
        .trim()
        .strip_prefix(OPEN_TAG)?
        .strip_suffix("\">")?
        .trim();
    (!name.is_empty() && !name.contains('"')).then_some(name)
}

/// A `</thread>` line: the tag alone on its line.
fn close_tag(line: &str) -> bool {
    line.trim() == "</thread>"
}

/// A line that may yet become a tag line as more text streams in (`<thread na`, `</thr`).
fn partial_tag(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && ("</thread>".starts_with(t)
            || OPEN_TAG.starts_with(t)
            || (t.starts_with(OPEN_TAG) && !t.contains('>')))
}

/// A thread section of `name` from its lines, without the blank lines around its text.
fn section(name: String, lines: &[&str], closed: bool) -> Section {
    let blank = |l: &&str| l.trim().is_empty();
    let start = lines.iter().position(|l| !blank(l)).unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|l| !blank(l))
        .map_or(start, |e| e + 1);
    Section::Thread {
        name,
        text: lines[start..end].join("\n"),
        closed,
    }
}

/// The orchestrator's reply `text` split into its thread sections (between a
/// `<thread name="NAME">` line and a `</thread>` line) and the text around them. A tag counts
/// only alone on its line, so one quoted inside a line (a worker's report) is text. Sections
/// do not nest: an opening tag inside a section closes it and opens the next. A `</thread>`
/// line outside a section is dropped, so a recognized tag never shows. A section open at the
/// end stays open while the reply streams in (`open`) and is closed once it is finished;
/// while it streams, a last line that may yet become a tag shows as an empty line until it
/// is whole ([`partial_tag`]), so a tag never flashes as text.
pub fn thread_sections(text: &str, open: bool) -> Vec<Section> {
    // Most replies have no tag at all, whole or partial: drawn as they are, every frame.
    if !text.contains('<') {
        return vec![Section::Plain(text.to_string())];
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if let Some(last) = lines.last_mut().filter(|l| open && partial_tag(l)) {
        *last = "";
    }
    let mut out = Vec::new();
    let mut plain: Vec<&str> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in lines {
        let opened = open_tag(line);
        if opened.is_none() && !close_tag(line) {
            match &mut current {
                Some((_, body)) => body.push(line),
                None => plain.push(line),
            }
            continue;
        }
        if let Some((name, body)) = current.take() {
            out.push(section(name, &body, true));
        } else if !plain.is_empty() {
            out.push(Section::Plain(plain.join("\n")));
            plain.clear();
        }
        current = opened.map(|n| (n.to_string(), Vec::new()));
    }
    match current {
        Some((name, body)) => out.push(section(name, &body, !open)),
        None if !plain.is_empty() => out.push(Section::Plain(plain.join("\n"))),
        None => {}
    }
    out
}

/// Narrowest pane a thread box is drawn in. Below it a section is its name on a row of its
/// own, then its text, with no frame.
const BOX_MIN_WIDTH: usize = 8;

/// Maximum collapsed conversation block height, including its frame and expansion row.
pub const BLOCK_MAX_HEIGHT: usize = 12;

/// The stable identity of a message, independent of its text, width, and streaming status.
fn block_key(entry: &Entry, index: usize, threads: &Threads) -> String {
    let thread = threads
        .state
        .and_then(|s| s.transcript.as_ref())
        .map_or("", |t| &t.thread);
    let (kind, seq) = match entry {
        Entry::Text { seq, .. } => ("text", *seq),
        Entry::TurnStart { seq, .. } => ("prompt", *seq),
        Entry::Notice { seq, .. } => ("notice", *seq),
        Entry::HubUpdate { seq, .. } => ("update", *seq),
        Entry::Tool { call_id, .. } => return format!("block:{thread}:tool:{call_id}"),
        Entry::TurnEnd { .. } => ("end", 0),
    };
    // Synthetic entries used by offscreen callers may have no event position.
    let id = if seq == 0 {
        format!("#{index}")
    } else {
        seq.to_string()
    };
    format!("block:{thread}:{kind}:{id}")
}

/// The block selected/opened by a row link; other transcript links keep their own behavior.
pub(crate) fn linked_block(link: &str) -> Option<&str> {
    let key = link.strip_prefix("toggle:").unwrap_or(link);
    key.starts_with("block:").then_some(key)
}

/// Cap a long block at its head while streaming and after completion alike. The footer is
/// drawing, excluded from copied text. An expanded block retains its footer after a resize.
fn long_rows(
    mut rows: Vec<TranscriptRow>,
    width: usize,
    key: String,
    threads: &Threads,
) -> Vec<TranscriptRow> {
    let Some(state) = threads.state.filter(|_| threads.boxes) else {
        return rows;
    };
    let expanded = state.view.long_expanded.contains(&key);
    let boxed = matches!(rows.first().map(|r| r.3), Some(Edge::Top(_)));
    let closed = boxed && rows.last().is_some_and(|r| r.3 == Edge::Bottom);
    let full_height = rows.len() + usize::from(boxed && !closed);
    if full_height <= BLOCK_MAX_HEIGHT && !expanded {
        return rows;
    }
    if closed {
        rows.pop();
    }
    let hidden = rows.len().saturating_sub(BLOCK_MAX_HEIGHT - 1);
    // Cut inside a line, the footer continues it (adding nothing to a copy, as it never
    // does): a place in the line's hidden rows comes after the rows kept, at the footer
    // ([`places`]).
    let mut join = Join::Skip;
    if !expanded {
        if let Some(Join::Space | Join::Glue) = rows.get(BLOCK_MAX_HEIGHT - 1).map(|r| r.1) {
            join = Join::Glue;
        }
        rows.truncate(BLOCK_MAX_HEIGHT - 1);
    }
    let selected = state.view.block.as_deref() == Some(&key);
    let frame = if selected {
        Style::default().fg(Color::Yellow)
    } else if boxed {
        rows.first()
            .and_then(|r| r.0.spans.first())
            .map_or(Style::default(), |s| s.style)
    } else {
        Style::default().fg(MUTED)
    };
    // Choose a complete hint by display width, so phone-sized panes keep the key visible:
    // Enter once it is selected and nothing is typed (with text, Enter sends it), else a
    // click.
    let short = if selected && state.view.input.is_empty() {
        "Enter"
    } else {
        "click"
    };
    let hints = if expanded {
        vec![
            format!("{short} to collapse"),
            format!("{short} collapse"),
            short.to_string(),
        ]
    } else {
        vec![
            format!("… {hidden} more lines · {short} to expand"),
            format!("… {hidden} more lines · {short} expand"),
            format!("… {hidden} more · {short}"),
            short.to_string(),
        ]
    };
    let room = width.saturating_sub(if boxed { 6 } else { 0 });
    let hint = hints
        .iter()
        .find(|h| crate::edit::width(h) <= room)
        .unwrap_or(hints.last().unwrap());
    let hint = truncate_cells(hint, room);
    let footer = if boxed {
        format!(
            "╰─ {hint} {}╯",
            "─".repeat(width.saturating_sub(crate::edit::width(&hint) + 5))
        )
    } else {
        hint
    };
    for row in &mut rows {
        row.2 = Some(key.clone());
        if selected && matches!(row.3, Edge::Top(_)) {
            for span in &mut row.0.spans {
                span.style = span.style.fg(Color::Yellow);
            }
        }
    }
    rows.push((
        Line::from(Span::styled(footer, frame)),
        join,
        Some(format!("toggle:{key}")),
        Edge::Bottom,
    ));
    rows
}

/// Apply the block limit to just-built rows, leaving their separating blank row alone.
fn limit_block(v: &mut View, start: usize, key: String, threads: &Threads) {
    let start = start + usize::from(v.out.get(start).is_some_and(|r| r.0.width() == 0));
    let rows = v.out.split_off(start.min(v.out.len()));
    v.out.extend(long_rows(rows, v.width, key, threads));
}

/// The rows of the thread section `name` with `text`, `width` cells wide (the pane's): a
/// rounded box in the thread's color ([`thread_color`]), dim grey for a name that matches no
/// thread, its top border titled with the thread's name and status (`╭─ web · running ──╮`,
/// the status dropped first when the title doesn't fit), the text wrapped inside, and its
/// bottom border once `closed` (a section still streaming in is open at the bottom). Title
/// and text are measured in display cells ([`wrap_rows`], [`truncate_cells`]), so every
/// framed row is exactly `width` cells, wide characters and combining marks included. Its
/// frame is marked ([`Edge`]) so a copy leaves it out. The box is the orchestrator's own
/// text: rounded and in a thread's color, it never looks like a card.
fn thread_box(
    name: &str,
    text: &str,
    closed: bool,
    width: usize,
    threads: &Threads,
) -> Vec<TranscriptRow> {
    let thread = threads.get(name);
    let frame = Style::default().fg(thread.map_or(Color::DarkGray, |t| thread_color(&t.id)));
    let label = thread.map_or(name, |t| t.name.as_str());
    let mut out = Vec::new();
    if width < BOX_MIN_WIDTH {
        push_text(
            &mut out,
            label,
            frame.add_modifier(Modifier::BOLD),
            None,
            width,
        );
        if !text.is_empty() {
            push_text(&mut out, text, Style::default(), None, width);
        }
        return out;
    }
    // `╭─ `, the title, a space, at least one `─`, `╮`.
    let room = width - 6;
    let mut title = vec![Span::styled(
        truncate_cells(label, room),
        frame.add_modifier(Modifier::BOLD),
    )];
    let label_w = crate::edit::width(label);
    if let Some((s, text)) = thread
        .map(|t| (t.status(), t.status_label()))
        .filter(|(_, text)| label_w + 3 + text.len() <= room)
    {
        title.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        title.push(Span::styled(text, Style::default().fg(status_color(s))));
    }
    let used = Line::from(title.clone()).width();
    let mut top = vec![Span::styled("╭─ ", frame)];
    top.extend(title);
    let fill = "─".repeat(width.saturating_sub(used + 5));
    top.push(Span::styled(format!(" {fill}╮"), frame));
    out.push((Line::from(top), Join::Newline, None, Edge::Top(used)));
    if !text.is_empty() {
        let inner = width - 4;
        // Its text's markdown rendered; the frame comes after the last span of a row.
        for r in crate::md::rows(text, inner, 0) {
            let used: usize = r
                .line
                .spans
                .iter()
                .map(|s| crate::edit::width(&s.content))
                .sum();
            let pad = " ".repeat(inner.saturating_sub(used));
            let mut spans = vec![Span::styled("│ ", frame)];
            spans.extend(r.line.spans.into_iter().filter(|s| !s.content.is_empty()));
            spans.push(Span::styled(format!("{pad} │"), frame));
            let edge = match u16::try_from(r.gutter).unwrap_or(u16::MAX) {
                0 => Edge::Bars,
                n => Edge::BarsIndent(n),
            };
            out.push((Line::from(spans), r.join, None, edge));
        }
    }
    if closed {
        let line = Line::from(Span::styled(format!("╰{}╯", "─".repeat(width - 2)), frame));
        // All frame: a copy across it joins the rows around it, no blank line between.
        out.push((line, Join::Skip, None, Edge::Bottom));
    }
    out
}

/// Pushes `text` wrapped to `width` onto `out` in `style`, each row linked to `link`.
fn push_text(
    out: &mut Vec<TranscriptRow>,
    text: &str,
    style: Style,
    link: Option<&str>,
    width: usize,
) {
    for (l, join) in wrap_rows(text, width) {
        out.push((
            Line::from(Span::styled(l, style)),
            join,
            link.map(str::to_string),
            Edge::None,
        ));
    }
}

/// The thread every call of a folded `group` is about ([`tool_thread`]), as the first call
/// names it; `None` when a call is about no thread or about another one.
fn tools_thread(group: &[&Entry], entries: &[Entry], threads: &Threads) -> Option<String> {
    let mut attns = None;
    let mut about: Option<(String, String)> = None;
    for e in group {
        let name = tool_thread(e, entries, threads, &mut attns)?;
        let key = threads
            .get(&name)
            .map_or_else(|| name.clone(), |t| t.id.clone());
        match &about {
            Some((_, k)) if *k != key => return None,
            Some(_) => {}
            None => about = Some((name, key)),
        }
    }
    about.map(|(name, _)| name)
}

/// The thread tool call `e` is about: the one its input or result names, or, for an answer
/// or a note on an attention item, the item's thread: open in `threads`' state, or named
/// next to the item's id in one of the hub's notices in `entries` (`attns`, read once).
fn tool_thread(
    e: &Entry,
    entries: &[Entry],
    threads: &Threads,
    attns: &mut Option<HashMap<String, String>>,
) -> Option<String> {
    let Entry::Tool {
        tool,
        summary,
        thread,
        ..
    } = e
    else {
        return None;
    };
    if let Some(t) = thread {
        return Some(t.clone());
    }
    if !matches!(
        tool.strip_prefix(TWR_TOOL),
        Some("respond_attention" | "annotate_attention")
    ) {
        return None;
    }
    let id = summary_str(summary, "attn_id")?;
    if let Some(item) = threads.state.and_then(|s| s.attention.get(&id)) {
        return (item.thread != HUB_THREAD).then(|| item.thread.clone());
    }
    attns
        .get_or_insert_with(|| notice_attns(entries))
        .get(&id)
        .cloned()
}

/// The attention ids the hub's notices in `entries` list, each with its thread's name
/// (`- thread=web reason=permission attn_id=… …`).
fn notice_attns(entries: &[Entry]) -> HashMap<String, String> {
    entries
        .iter()
        .filter_map(|e| match e {
            Entry::TurnStart {
                prompt,
                notice: true,
                ..
            } => Some(prompt),
            _ => None,
        })
        .flat_map(|p| p.lines())
        .filter_map(|l| {
            let name = notice_line_thread(l)?;
            let id = l.split(' ').find_map(|w| w.strip_prefix("attn_id="))?;
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

/// The thread a line of a hub notice names (`- thread=web …`).
fn notice_line_thread(line: &str) -> Option<&str> {
    line.strip_prefix("- thread=")?
        .split(' ')
        .next()
        .filter(|n| !n.is_empty())
}

/// The thread every update of the hub notice `prompt` is about: each of its update lines
/// (`- …`) names the same thread (`- thread=web …`). `None` for any other line (`- ... and 3
/// more`) or another thread.
fn notice_thread(prompt: &str) -> Option<&str> {
    let mut about = None;
    for l in prompt.lines().filter(|l| l.starts_with("- ")) {
        let name = notice_line_thread(l)?;
        if about.is_some_and(|a| a != name) {
            return None;
        }
        about = Some(name);
    }
    about
}

/// [`transcript_rows`] with `collapse` applied, each row with the folded group a click on it
/// expands and its frame cells ([`TranscriptRow`]), drawn the way Claude Code draws a
/// conversation: a prompt after `❯` on a grey band, each reply after `●` with its markdown
/// rendered ([`crate::md`]), tool calls as one dim line saying what they did, the end of a turn
/// after `✻`, and a blank row between blocks. With `threads.boxes`, each thread section of the
/// agent's text is a box ([`thread_box`]); a folded line whose calls or updates are all about
/// one thread names it. The hub's updates are one row each (a request and how it ended on one
/// row), and a run of answered requests folds into one.
pub fn transcript_view(
    entries: &[Entry],
    width: u16,
    collapse: &Collapse,
    threads: &Threads,
) -> Vec<TranscriptRow> {
    transcript_view_spans(entries, width, collapse, threads).0
}

/// The rows the entries of a transcript drew ([`transcript_view_spans`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spans {
    /// Each entry's rows, not the blank row before it: the first entry of a folded group has
    /// its marker row, the others none, at that row; an entry that draws no row has none,
    /// where the next one starts.
    pub entries: Vec<Range<usize>>,
    /// The first row of each section of an entry after its first (a reply's thread box, or
    /// its text after one): a place is counted from its section's first row ([`Anchor`]),
    /// so a long block collapsed or expanded moves no place in the entry's other sections.
    pub sections: Vec<usize>,
}

/// [`transcript_view`] and the rows each entry drew ([`Spans`]).
pub fn transcript_view_spans(
    entries: &[Entry],
    width: u16,
    collapse: &Collapse,
    threads: &Threads,
) -> (Vec<TranscriptRow>, Spans) {
    let mut spans = Vec::with_capacity(entries.len());
    let mut sections = Vec::new();
    let width = usize::from(width.max(1));
    let mut v = View {
        out: Vec::new(),
        prev: None,
        blank: None,
        width,
    };
    let requests = threads
        .state
        .filter(|_| threads.boxes)
        .map_or_else(Vec::new, AppState::inline_requests);
    // The update that raised each request still waiting (an ended one, or an end on its own,
    // never stands for a request raised again under its id).
    let last_update: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Entry::HubUpdate {
                attn_id: Some(id),
                outcome: None,
                resolution: false,
                ..
            } => Some((id.as_str(), i)),
            _ => None,
        })
        .collect();
    // Requests raised before this conversation existed still remain directly accessible.
    for item in requests
        .iter()
        .filter(|item| !last_update.contains_key(item.attn_id.as_str()))
    {
        v.block(Kind::Block);
        v.out
            .extend(request_rows(threads.state.unwrap(), item, width as u16));
    }
    let mut i = 0;
    while i < entries.len() {
        let start = v.out.len();
        match &entries[i] {
            Entry::HubUpdate {
                attn_id: Some(id),
                outcome: None,
                ..
            } if last_update.get(id.as_str()) == Some(&i)
                && requests.iter().any(|item| item.attn_id == *id) =>
            {
                let item = requests.iter().find(|item| item.attn_id == *id).unwrap();
                v.block(Kind::Block);
                v.out
                    .extend(request_rows(threads.state.unwrap(), item, width as u16));
            }
            // A run of answered requests: one row, unless expanded.
            Entry::HubUpdate {
                outcome: Some(_),
                seq,
                ..
            } if collapse.on => {
                let n = entries[i..]
                    .iter()
                    .take_while(|e| {
                        matches!(
                            e,
                            Entry::HubUpdate {
                                outcome: Some(_),
                                ..
                            }
                        )
                    })
                    .count();
                // By the first update's event: an attention id may come again in another run.
                let key = format!("answers:{seq}");
                if n >= ANSWERS_FOLD && collapse.folded(&key) {
                    v.block(Kind::Compact);
                    let run: Vec<&Entry> = entries[i..i + n].iter().collect();
                    let row = answers_marker(&run, threads, width);
                    let marker = v.out.len();
                    v.out.push((row, Join::Newline, Some(key), Edge::Gutter(2)));
                    spans.push(v.drawn(start));
                    spans.extend((1..n).map(|_| marker..marker));
                } else {
                    for (offset, e) in entries[i..i + n].iter().enumerate() {
                        let from = v.out.len();
                        v.block(Kind::Compact);
                        hub_update_rows(&mut v, e, threads);
                        limit_block(&mut v, from, block_key(e, i + offset, threads), threads);
                        spans.push(v.drawn(from));
                    }
                }
                i += n;
                continue;
            }
            Entry::Tool { call_id, .. } if collapse.on => {
                let n = entries[i..]
                    .iter()
                    .take_while(|e| matches!(e, Entry::Tool { .. }))
                    .count();
                let group: Vec<&Entry> = entries[i..i + n].iter().collect();
                let key = format!("tools:{call_id}");
                v.block(Kind::Block);
                if collapse.folded(&key) {
                    let about = tools_thread(&group, entries, threads);
                    let row = tools_row(&group, about.as_deref(), threads, width);
                    let marker = v.out.len();
                    v.out.push((row, Join::Newline, Some(key), Edge::Gutter(2)));
                    spans.push(v.drawn(start));
                    spans.extend((1..n).map(|_| marker..marker));
                } else {
                    let mut from = start;
                    for (offset, e) in group.into_iter().enumerate() {
                        let at = v.out.len();
                        tool_rows(&mut v, e);
                        limit_block(&mut v, at, block_key(e, i + offset, threads), threads);
                        spans.push(v.drawn(from));
                        from = v.out.len();
                    }
                }
                i += n;
                continue;
            }
            Entry::TurnStart {
                turn,
                notice: true,
                prompt,
                ..
            } => {
                let key = format!(
                    "notice:{}",
                    turn.map_or_else(|| format!("#{i}"), |t| t.to_string())
                );
                v.block(Kind::Compact);
                if collapse.folded(&key) {
                    let about = notice_thread(prompt);
                    let row = marker_row(NOTICE_MARKER, about, threads, width);
                    v.out.push((row, Join::Newline, Some(key), Edge::Gutter(2)));
                } else {
                    let at = v.out.len();
                    let dim = Style::default().fg(Color::DarkGray);
                    v.marked(
                        2,
                        ("", dim),
                        Vec::new(),
                        &format!("{NOTICE_MARKER}:\n{prompt}"),
                        dim,
                    );
                    limit_block(&mut v, at, block_key(&entries[i], i, threads), threads);
                }
            }
            Entry::TurnEnd {
                result: TurnResult::Completed,
                duration_ms,
            } if collapse.quiet_ends && duration_ms.is_none_or(|ms| ms < QUIET_END_MS) => {}
            Entry::Text { text, .. } if threads.boxes => {
                v.block(Kind::Block);
                let mut first = true;
                for (section, s) in
                    thread_sections(text, matches!(entries[i], Entry::Text { open: true, .. }))
                        .into_iter()
                        .enumerate()
                {
                    let at = v.out.len();
                    if section > 0 {
                        sections.push(at);
                    }
                    match s {
                        Section::Plain(t) => {
                            v.message(&t, first);
                            first = false;
                        }
                        Section::Thread { name, text, closed } => {
                            v.out
                                .extend(thread_box(&name, &text, closed, width, threads));
                            first = false;
                        }
                    }
                    limit_block(
                        &mut v,
                        at,
                        format!("{}:{section}", block_key(&entries[i], i, threads)),
                        threads,
                    );
                }
            }
            e => {
                entry_rows(&mut v, e, threads);
                limit_block(&mut v, start, block_key(e, i, threads), threads);
            }
        }
        spans.push(v.drawn(start));
        i += 1;
    }
    let spans = Spans {
        entries: spans,
        sections,
    };
    (v.out, spans)
}

/// The hub's answered requests a run of this many or more folds into one row.
const ANSWERS_FOLD: usize = 3;

/// The orchestrator conversation leaves out a completed turn's end, unless it took this long.
const QUIET_END_MS: u64 = 60_000;

/// The dim grey of folded lines and turn ends (Claude Code's).
const MUTED: Color = Color::Indexed(246);

/// What separates blocks in the main pane: a blank row, except between one-row updates and
/// notices, which follow one another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A prompt, a reply, a group of tool calls, a box, a turn's end: a blank row around it.
    Block,
    /// A one-row hub update or notice.
    Compact,
}

/// The rows of the main pane being built ([`transcript_view`]).
struct View {
    out: Vec<TranscriptRow>,
    /// What the last rows were.
    prev: Option<Kind>,
    /// The blank row between blocks put in last ([`View::block`]).
    blank: Option<usize>,
    width: usize,
}

impl View {
    /// The indentation of replies and prompts: none in a pane too narrow for it.
    fn indent(&self) -> usize {
        if self.width > 4 {
            2
        } else {
            0
        }
    }

    /// Starts a `kind` of rows: a blank row first when it or the rows before are a block.
    fn block(&mut self, kind: Kind) {
        let after_text = self.out.last().is_some_and(|r| r.0.width() > 0);
        if after_text && (kind == Kind::Block || self.prev == Some(Kind::Block)) {
            self.blank = Some(self.out.len());
            self.out
                .push((Line::default(), Join::Newline, None, Edge::None));
        }
        self.prev = Some(kind);
    }

    /// The rows drawn from row `start` on, but for a blank row between blocks there: an
    /// entry's text starts on its first line whether or not one came before it.
    fn drawn(&self, start: usize) -> Range<usize> {
        let from = if self.blank == Some(start) {
            start + 1
        } else {
            start
        };
        from.min(self.out.len())..self.out.len()
    }

    /// A row led by `mark` (`✓ `, `! `, `✻ `, `● `, ...) after `at` cells: then `lead` (a
    /// thread's name, a tool's), then `text`, each of its lines wrapped under the mark. The
    /// mark and the indentation are left out of a copy.
    fn marked(
        &mut self,
        at: usize,
        mark: (&str, Style),
        lead: Vec<(String, Style)>,
        text: &str,
        style: Style,
    ) {
        let hang = (at + crate::edit::width(mark.0)).min(self.width / 2);
        let at = at.min(self.width / 2);
        for (k, line) in text.split('\n').enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut spans = Vec::new();
            if k == 0 {
                spans.push((mark.0.to_string(), mark.1));
                spans.extend(lead.iter().cloned());
            }
            if !line.is_empty() {
                spans.push((line.to_string(), style));
            }
            if spans.is_empty() {
                continue;
            }
            let first = if k == 0 { at } else { hang };
            let mut rows = crate::md::wrap(spans, self.width, first, hang);
            if let (0, Some(r)) = (k, rows.first_mut()) {
                r.gutter = (at + crate::edit::width(mark.0)).min(r.line.width());
            }
            for r in rows {
                self.out
                    .push((r.line, r.join, None, Edge::Gutter(r.gutter as u16)));
            }
        }
    }

    /// An agent's text, its markdown rendered, after `●` when it is the start of a reply.
    fn message(&mut self, text: &str, bullet: bool) {
        let indent = self.indent();
        // A blank line before the text (after a box) is kept.
        if text.starts_with(['\n', '\r']) && self.out.last().is_some_and(|r| r.0.width() > 0) {
            self.out
                .push((Line::default(), Join::Newline, None, Edge::None));
        }
        let mut rows = crate::md::rows(text, self.width, indent);
        if bullet && indent == 2 {
            // The bullet takes the first two cells of the indentation, nothing else moves.
            if let Some(first) = rows.iter_mut().find(|r| r.line.width() > 0) {
                if let Some(lead) = first
                    .line
                    .spans
                    .first_mut()
                    .filter(|s| s.content.starts_with("  "))
                {
                    lead.content = format!("● {}", &lead.content[2..]).into();
                }
            }
        }
        for r in rows {
            self.out
                .push((r.line, r.join, None, Edge::Gutter(r.gutter as u16)));
        }
    }

    /// A prompt: `❯` and its text on a grey band across the pane.
    fn prompt(&mut self, prompt: &str, from: TurnFrom, withdrawn: bool) {
        // One the orchestrator sent, or one the user took back, says so on a dim row above
        // it: drawn, never copied.
        let label = match (withdrawn, from) {
            (true, _) => Some("taken back"),
            (false, TurnFrom::Orchestrator) => Some("from the orchestrator"),
            _ => None,
        };
        if let Some(label) = label {
            let style = Style::default().fg(MUTED).add_modifier(Modifier::ITALIC);
            let row = Line::from(cut_spans(vec![Span::styled(label, style)], self.width));
            self.out.push((row, Join::Skip, None, Edge::Bottom));
        }
        // Taken back: struck out and dimmed.
        let band = match withdrawn {
            true => Style::default()
                .fg(Color::Indexed(245))
                .bg(Color::Indexed(237)),
            false => Style::default()
                .fg(Color::Indexed(231))
                .bg(Color::Indexed(237)),
        };
        let struck = Style::default().add_modifier(Modifier::CROSSED_OUT);
        let indent = self.indent();
        let rows = crate::md::plain_rows(prompt, self.width, indent);
        for (k, r) in rows.into_iter().enumerate() {
            let mut line = r.line;
            let mut gutter = r.gutter;
            if k == 0 && indent == 2 {
                // The mark is left out of a copy.
                gutter = gutter.max(2);
                // (A blank first line has no indentation span to replace.)
                if line.spans.is_empty() {
                    line.spans.push(Span::raw("  "));
                }
                // The mark takes the first two cells; the line's own indentation stays.
                let rest = line.spans[0].content.get(2..).unwrap_or("").to_string();
                line.spans[0] = Span::styled("❯ ", Style::default().fg(MUTED));
                if !rest.is_empty() {
                    line.spans.insert(1, Span::raw(rest));
                }
            }
            if withdrawn {
                for s in &mut line.spans {
                    s.style = s.style.patch(struck);
                }
            }
            let used: usize = line
                .spans
                .iter()
                .map(|s| crate::edit::width(&s.content))
                .sum();
            let pad = self.width.saturating_sub(used);
            line.spans.push(Span::raw(" ".repeat(pad)));
            self.out
                .push((line.style(band), r.join, None, Edge::Gutter(gutter as u16)));
        }
    }
}

/// The rows of one entry (not a folded group).
fn entry_rows(v: &mut View, e: &Entry, threads: &Threads) {
    match e {
        Entry::TurnStart {
            from,
            prompt,
            withdrawn,
            ..
        } => {
            v.block(Kind::Block);
            v.prompt(prompt, *from, *withdrawn);
        }
        Entry::Text { text, .. } => {
            v.block(Kind::Block);
            v.message(text, true);
        }
        Entry::HubUpdate { .. } => {
            v.block(Kind::Compact);
            hub_update_rows(v, e, threads);
        }
        Entry::Tool { .. } => {
            v.block(Kind::Block);
            tool_rows(v, e);
        }
        Entry::TurnEnd {
            result,
            duration_ms,
        } => {
            v.block(Kind::Block);
            let took = duration_ms.map(duration_text);
            let (text, color) = match (result, took) {
                (TurnResult::Completed, Some(t)) => (format!("✻ Worked for {t}"), MUTED),
                (TurnResult::Completed, None) => ("✻ Done".to_string(), MUTED),
                (TurnResult::Error, Some(t)) => {
                    (format!("✗ The turn failed after {t}"), Color::Red)
                }
                (TurnResult::Error, None) => ("✗ The turn failed".to_string(), Color::Red),
                (TurnResult::Interrupted, Some(t)) => {
                    (format!("! Interrupted after {t}"), Color::Yellow)
                }
                (TurnResult::Interrupted, None) => ("! Interrupted".to_string(), Color::Yellow),
            };
            let style = Style::default().fg(color);
            let (mark, rest) = text.split_at(text.find(' ').map_or(0, |i| i + 1));
            v.marked(0, (mark, style), Vec::new(), rest, style);
        }
        Entry::Notice { level, text, .. } => {
            v.block(Kind::Compact);
            let (mark, color) = match level {
                NoticeLevel::Info => ("· ", Color::DarkGray),
                NoticeLevel::Attention => ("! ", Color::Yellow),
                NoticeLevel::Error => ("✗ ", Color::Red),
            };
            let style = Style::default().fg(color);
            v.marked(0, (mark, style), Vec::new(), text, style);
        }
    }
}

/// `ms` as Claude Code says how long a turn worked: `3.2s`, `41m 19s`, `1h 2m`.
pub fn duration_text(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{:.1}s", ms as f64 / 1000.0),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// An expanded tool call: `● Tool(input)`, the dot green once it succeeded, red if it failed
/// (with `⎿ failed` under it), grey while it runs.
fn tool_rows(v: &mut View, e: &Entry) {
    let Entry::Tool {
        tool, summary, ok, ..
    } = e
    else {
        return;
    };
    let dot = match ok {
        None => Color::DarkGray,
        Some(true) => Color::Green,
        Some(false) => Color::Red,
    };
    let name = match tool.strip_prefix(TWR_TOOL) {
        Some(action) => format!("twr {action}"),
        None => tool.clone(),
    };
    v.marked(
        0,
        ("● ", Style::default().fg(dot)),
        vec![(name, Style::default().add_modifier(Modifier::BOLD))],
        &format!("({summary})"),
        Style::default(),
    );
    if *ok == Some(false) {
        let red = Style::default().fg(Color::Red);
        v.marked(2, ("⎿  ", red), Vec::new(), "failed", red);
    }
}

/// What a tool does, as a folded line says it: its verb and what it counts (`ran`, `shell
/// command`); an empty noun when it is said once, uncounted (`listed the threads`).
fn tool_phrase(tool: &str) -> (&'static str, &'static str) {
    if let Some(action) = tool.strip_prefix(TWR_TOOL) {
        return match action {
            "list_threads" => ("listed the threads", ""),
            "get_thread" => ("checked", "thread"),
            "spawn_thread" => ("started", "thread"),
            "get_spawn" => ("checked", "thread start"),
            "send" => ("sent", "message"),
            "respond_attention" => ("answered", "request"),
            "set_mode" => ("changed", "mode"),
            "set_model" => ("changed", "model"),
            "annotate_attention" => ("added", "note"),
            "park" => ("parked", "thread"),
            "resume" => ("resumed", "thread"),
            "delete_thread" => ("asked to delete", "thread"),
            "list_trash" => ("listed the trash", ""),
            "get_usage" => ("checked the usage", ""),
            "stop" => ("stopped", "thread"),
            "read_transcript" => ("read", "transcript"),
            "subscribe_control" => ("watched the hub", ""),
            "write_report" => ("wrote", "report"),
            "read_context" => ("read the context", ""),
            "set_context" => ("updated the context", ""),
            _ => ("used", "twr tool"),
        };
    }
    match tool {
        "Bash" | "shell" | "BashOutput" => ("ran", "shell command"),
        "Read" => ("read", "file"),
        "Write" => ("wrote", "file"),
        "Edit" | "MultiEdit" => ("edited", "file"),
        "apply_patch" => ("applied", "patch"),
        "NotebookEdit" => ("edited", "notebook"),
        "Grep" | "Glob" => ("searched for", "pattern"),
        "WebFetch" => ("fetched", "page"),
        "WebSearch" | "web_search" => ("ran", "web search"),
        "Task" | "Agent" => ("ran", "agent"),
        "TodoWrite" => ("updated the todo list", ""),
        _ => ("called", "tool"),
    }
}

/// `noun` for `n` of it: `1 file`, `2 files`, `2 web searches`.
fn counted(n: usize, noun: &str) -> String {
    if n == 1 {
        return noun.to_string();
    }
    if noun.ends_with("ch") || noun.ends_with("sh") || noun.ends_with('s') || noun.ends_with('x') {
        format!("{noun}es")
    } else {
        format!("{noun}s")
    }
}

/// What a folded group of tool calls did, as plain text: `Ran 3 shell commands, read 2 files,
/// 1 failed (ctrl+o to expand)`, with `…` while one still runs.
pub fn tools_marker(tools: &[&Entry]) -> String {
    tools_spans(tools)
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// [`tools_marker`] in spans: dim, the counts bold, the failures red.
fn tools_spans(tools: &[&Entry]) -> Vec<Span<'static>> {
    let mut phrases: Vec<((&str, &str), usize)> = Vec::new();
    let (mut failed, mut running) = (0, false);
    for e in tools {
        let Entry::Tool { tool, ok, .. } = e else {
            continue;
        };
        let p = tool_phrase(tool);
        match phrases.iter_mut().find(|(q, _)| *q == p) {
            Some((_, n)) => *n += 1,
            None => phrases.push((p, 1)),
        }
        failed += usize::from(*ok == Some(false));
        running |= ok.is_none();
    }
    let dim = Style::default().fg(MUTED);
    let bold = dim.add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    for (k, ((verb, noun), n)) in phrases.into_iter().enumerate() {
        let verb = if k == 0 {
            let mut c = verb.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        } else {
            format!(", {verb}")
        };
        if noun.is_empty() {
            spans.push(Span::styled(verb, dim));
        } else {
            spans.push(Span::styled(format!("{verb} "), dim));
            spans.push(Span::styled(n.to_string(), bold));
            spans.push(Span::styled(format!(" {}", counted(n, noun)), dim));
        }
    }
    if failed > 0 {
        spans.push(Span::styled(
            format!(", {failed} failed"),
            Style::default().fg(Color::Red),
        ));
    }
    if running {
        spans.push(Span::styled("…", dim));
    }
    spans.push(Span::styled(
        " (ctrl+o to expand)",
        Style::default().fg(Color::DarkGray),
    ));
    spans
}

/// The row of a folded group of tool calls ([`tools_spans`]), after the thread they are all
/// about when there is one (`web · Checked 1 thread, sent 1 message`), cut to `width`.
fn tools_row(
    group: &[&Entry],
    about: Option<&str>,
    threads: &Threads,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(about_spans(about, threads));
    let mut said = tools_spans(group);
    // What failed, what still runs and the hint keep their room: the verbs give way.
    let tail_from = said
        .iter()
        .position(|s| s.content.starts_with(", ") && s.style.fg == Some(Color::Red))
        .or_else(|| said.iter().position(|s| s.content == "…"))
        .unwrap_or(said.len().saturating_sub(1));
    let tail = said.split_off(tail_from);
    spans.extend(said);
    let tail_w: usize = tail.iter().map(|s| crate::edit::width(&s.content)).sum();
    let mut line = cut_spans(spans, width.saturating_sub(tail_w));
    line.extend(tail);
    Line::from(cut_spans(line, width))
}

/// A thread's name in its color, then ` · `, for a folded line about it.
fn about_spans(about: Option<&str>, threads: &Threads) -> Vec<Span<'static>> {
    let Some(name) = about else {
        return Vec::new();
    };
    let thread = threads.get(name);
    let label = thread.map_or(name, |t| t.name.as_str()).to_string();
    let color = thread.map_or(Color::DarkGray, |t| thread_color(&t.id));
    vec![
        Span::styled(label, Style::default().fg(color)),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
    ]
}

/// The row of a folded hub notice turn: `· hub update`, after the thread it is about.
fn marker_row(marker: &str, about: Option<&str>, threads: &Threads, width: usize) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = vec![Span::raw("  ")];
    spans.extend(about_spans(about, threads));
    spans.push(Span::styled(
        format!("{} (ctrl+o to expand)", marker.trim_start_matches("· ")),
        dim,
    ));
    Line::from(cut_spans(spans, width))
}

/// What the hub's update `text` says ([`crate::app::Entry::HubUpdate`], written by the hub's
/// `conversation_updates`): about whom, what happened and its detail.
#[derive(Debug, PartialEq)]
enum UpdateShape<'a> {
    /// `<name> needs permission: <summary>` and the like: a request the thread raised.
    Request {
        name: &'a str,
        what: &'a str,
        summary: &'a str,
    },
    /// `<name> finished (<grade>): <summary>`: a report.
    Finished {
        name: &'a str,
        grade: &'a str,
        summary: &'a str,
    },
    /// Anything else, as it is.
    Other(&'a str),
}

impl<'a> UpdateShape<'a> {
    fn of(text: &'a str) -> Self {
        if let Some((name, rest)) = text.split_once(" finished (") {
            if let Some((grade, summary)) = rest.split_once("): ") {
                if !name.contains(' ') {
                    return UpdateShape::Finished {
                        name,
                        grade,
                        summary,
                    };
                }
            }
        }
        for what in [
            "needs permission",
            "asks",
            "failed",
            "needs review",
            "is stalled",
        ] {
            if let Some((name, summary)) = text.split_once(&format!(" {what}: ")) {
                if !name.contains(' ') {
                    return UpdateShape::Request {
                        name,
                        what,
                        summary,
                    };
                }
            }
        }
        UpdateShape::Other(text)
    }
}

/// How a request of kind `what` (`needs permission`, `asks`, ...) ended, from `outcome`
/// ([`crate::app::request_outcome`]: `answered: <answer>` or `closed`): `allowed`, `denied`
/// and `allowed always` for a permission, `dismissed` for an item dismissed (answered
/// `cancel`), `answered <answer>` otherwise, `closed` when nobody answered; its mark and color.
fn outcome_words(what: &str, outcome: &str) -> (String, &'static str, Color) {
    let Some(answer) = outcome.strip_prefix("answered: ") else {
        return ("closed".into(), "·", Color::DarkGray);
    };
    match (what, answer) {
        (_, "cancel") => ("dismissed".into(), "·", Color::DarkGray),
        ("needs permission", "allow") => ("allowed".into(), "✓", Color::Green),
        ("needs permission", "allow_always") => ("allowed always".into(), "✓", Color::Green),
        ("needs permission", "deny") => ("denied".into(), "✗", Color::Red),
        (_, other) => (format!("answered {other}"), "✓", Color::Green),
    }
}

/// The rows of one hub update: one row (a request with how it ended, a notice), or a report
/// as a block of its own: `●`, the thread, its grade, then its summary.
fn hub_update_rows(v: &mut View, e: &Entry, threads: &Threads) {
    let Entry::HubUpdate {
        text,
        user_message,
        outcome,
        about,
        ..
    } = e
    else {
        return;
    };
    if let Some(message) = user_message {
        v.block(Kind::Block);
        v.prompt(message, TurnFrom::User, false);
        v.block(Kind::Compact);
    }
    let dim = Style::default().fg(Color::DarkGray);
    let color = |name: &str| {
        let id = about
            .as_deref()
            .or_else(|| threads.get(name).map(|t| t.id.as_str()));
        Style::default().fg(id.map_or(Color::Reset, thread_color))
    };
    match (UpdateShape::of(text), outcome) {
        (
            UpdateShape::Request {
                name,
                what,
                summary,
            },
            Some(outcome),
        ) => {
            let (words, mark, mc) = outcome_words(what, outcome);
            // (The answer can hold line breaks: it goes with the text, which keeps them.)
            v.marked(
                0,
                (&format!("{mark} "), Style::default().fg(mc)),
                vec![(name.to_string(), color(name))],
                &format!(" {words} · {summary}"),
                dim,
            );
        }
        (
            UpdateShape::Request {
                name,
                what,
                summary,
            },
            None,
        ) => {
            // Only a permission or a question waits for an answer; the hub writes no end for
            // the others.
            let (mark, mc) = match what {
                "needs permission" | "asks" => ("! ", Color::Yellow),
                "failed" => ("✗ ", Color::Red),
                _ => ("· ", Color::Yellow),
            };
            v.marked(
                0,
                (mark, Style::default().fg(mc)),
                vec![
                    (name.to_string(), color(name)),
                    (format!(" {what}: "), Style::default().fg(mc)),
                ],
                summary,
                Style::default(),
            );
        }
        (
            UpdateShape::Finished {
                name,
                grade,
                summary,
            },
            _,
        ) => {
            v.block(Kind::Block);
            let gc = match grade {
                "success" => Color::Green,
                "failure" | "failed" => Color::Red,
                _ => Color::Yellow,
            };
            v.marked(
                0,
                ("● ", Style::default().fg(gc)),
                vec![
                    (name.to_string(), color(name).add_modifier(Modifier::BOLD)),
                    (format!(" finished · {grade}"), dim),
                ],
                "",
                dim,
            );
            v.message(summary, false);
            v.prev = Some(Kind::Block);
        }
        (UpdateShape::Other(text), Some(outcome)) => {
            let (words, mark, mc) = outcome_words("", outcome);
            v.marked(
                0,
                (&format!("{mark} "), Style::default().fg(mc)),
                Vec::new(),
                &format!("{text} ({words})"),
                dim,
            );
        }
        (UpdateShape::Other(text), None) => v.marked(0, ("· ", dim), Vec::new(), text, dim),
    }
}

/// The row a folded run of ended requests shows, saying how they ended: `✓ 8 requests · 7
/// allowed, 1 closed · web` (`✗` when one was denied, `·` when none was answered), linked to
/// expand it.
fn answers_marker(run: &[&Entry], threads: &Threads, width: usize) -> Line<'static> {
    let mut names: Vec<String> = Vec::new();
    let mut tally: Vec<(String, usize)> = Vec::new();
    let (mut denied, mut answered) = (false, false);
    for e in run {
        let Entry::HubUpdate {
            text,
            outcome: Some(o),
            ..
        } = e
        else {
            continue;
        };
        let what = match UpdateShape::of(text) {
            UpdateShape::Request { name, what, .. } => {
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
                what
            }
            _ => "",
        };
        let (words, mark, _) = outcome_words(what, o);
        denied |= mark == "✗";
        answered |= mark == "✓";
        // `answered yes` and `answered no` are both answers.
        let key = if words.starts_with("answered ") {
            "answered".to_string()
        } else {
            words
        };
        match tally.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => tally.push((key, 1)),
        }
    }
    let dim = Style::default().fg(MUTED);
    let (mark, mc) = match (denied, answered) {
        (true, _) => ("✗ ", Color::Red),
        (false, true) => ("✓ ", Color::Green),
        _ => ("· ", Color::DarkGray),
    };
    let how = tally
        .iter()
        .map(|(k, n)| format!("{n} {k}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut spans = vec![
        Span::styled(mark, Style::default().fg(mc)),
        Span::styled(run.len().to_string(), dim.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" requests · {how}"), dim),
    ];
    if let [name] = names.as_slice() {
        spans.push(Span::styled(" · ", dim));
        spans.extend(about_spans(Some(name), threads).into_iter().take(1));
    }
    spans.push(Span::styled(
        " (ctrl+o to expand)",
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(cut_spans(spans, width))
}

/// Greedy word wrap by display width; words wider than `width` are split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_rows(text, width).into_iter().map(|(l, _)| l).collect()
}

/// [`wrap`], each row with how it continues the row above: the first row of each line of
/// `text` starts a line, a row wrapped at a space continues after that (undrawn) space, and
/// the rest of a word split for being too long continues it directly.
///
/// Rows are measured in cells as ratatui draws them ([`clusters`], [`cluster_width`]): a wide
/// character (CJK, emoji) takes two, a combining mark none. A word is split between grapheme
/// clusters, never inside one, and a cluster with no room at a row's end starts the next row,
/// so no row is wider than `width` (but for a single cluster wider than a row).
fn wrap_rows(text: &str, width: usize) -> Vec<(String, Join)> {
    let mut out = Vec::new();
    for raw in text.split('\n') {
        // `line` is Some once it holds at least one word (possibly empty, to keep indentation),
        // with its width in cells.
        let mut line: Option<(String, usize, Join)> = None;
        let mut join = Join::Newline;
        for word in raw.split(' ') {
            let wlen = crate::edit::width(word);
            if let Some((l, used, _)) = &mut line {
                if *used + 1 + wlen <= width {
                    l.push(' ');
                    l.push_str(word);
                    *used += 1 + wlen;
                    continue;
                }
                out.extend(line.take().map(|(l, _, j)| (l, j)));
                join = Join::Space;
            }
            let mut row = String::new();
            let mut used = 0;
            if wlen > width {
                for (_, g) in clusters(word) {
                    let w = cluster_width(g);
                    if used > 0 && used + w > width {
                        out.push((std::mem::take(&mut row), join));
                        join = Join::Glue;
                        used = 0;
                    }
                    row.push_str(g);
                    used += w;
                }
            } else {
                row.push_str(word);
                used = wlen;
            }
            line = Some((row, used, join));
        }
        out.push(line.map(|(l, _, j)| (l, j)).unwrap_or_default());
    }
    out
}

/// The color of where a typed line goes, for the main pane's border and the input box's
/// border and title text: cyan for the orchestrator, magenta for a dive into a worker (the
/// terminal's own, which its theme keeps legible on its background).
pub fn target_color(dive: bool) -> Color {
    if dive {
        Color::Magenta
    } else {
        Color::Cyan
    }
}

/// The fill of where a typed line goes, for the main pane's header band and the input box's
/// label: black on a light cyan (orchestrator) or a light magenta (dive). Fixed 256-color
/// values, not the terminal's own: some themes make their cyan or black dark enough that
/// black on cyan would not read; these keep the same contrast (over 8:1) on dark and light
/// terminals alike.
pub fn target_fill(dive: bool) -> Style {
    Style::default()
        .fg(Color::Indexed(16))
        .bg(Color::Indexed(if dive { 176 } else { 80 }))
}

/// The main pane's header for `width` columns: its label, `ORCHESTRATOR` or, in a dive,
/// `talking to <name>`, and the details that fit after it (two columns apart): in a dive the
/// runtime, model, mode and what Esc does ([`esc_words`]: how to get back, when the next Esc
/// leaves the dive), shortened as needed; in the orchestrator conversation the runtime and the
/// full model id the runtime reports, cut to fit. Before an orchestrator thread exists, the
/// one the hub would start, which starts with the first message. A label wider than `width`
/// is cut, with no details; in a dive, before that, the label is the thread's name alone (cut
/// as needed) so that what Esc does still fits.
pub fn main_title(state: &AppState, width: usize) -> (String, String) {
    let label = match state.dived() {
        Some(t) => format!("talking to {}", t.name),
        None => "ORCHESTRATOR".to_string(),
    };
    let (esc_long, back) = esc_words(state);
    if let Some(t) = state.dived() {
        let room = width.saturating_sub(back.len() + 2);
        if label.chars().count() > room && room >= 1 {
            return (truncate(&t.name, room), back.to_string());
        }
    }
    let used = label.chars().count();
    if used > width {
        return (truncate(&label, width), String::new());
    }
    let room = width.saturating_sub(used + 2);
    let detail = match state.dived() {
        Some(t) => {
            let rt = if t.runtime.is_empty() {
                "?"
            } else {
                t.runtime.as_str()
            };
            let model = model_label(t);
            let mode = if t.mode.is_empty() {
                String::new()
            } else {
                format!(" · mode {}", t.mode)
            };
            [
                format!("{rt} · {model}{mode} · {esc_long}"),
                format!("{rt} · {model}{mode} · {back}"),
                format!("{} · {back}", mode.trim_start_matches(" · ")),
                back.to_string(),
            ]
            .into_iter()
            .map(|v| v.trim_start_matches(" · ").to_string())
            .find(|v| v.chars().count() <= room)
            .unwrap_or_default()
        }
        None => {
            let full = match (state.orchestrator(), &state.view.orch_default) {
                (Some(o), _) => format!(
                    "{} · {}",
                    if o.runtime.is_empty() {
                        "?"
                    } else {
                        &o.runtime
                    },
                    model_full(o)
                ),
                (None, Some((rt, model))) => format!(
                    "{rt} · {} · starts with your first message",
                    model.as_deref().unwrap_or("default model")
                ),
                (None, None) => "starts with your first message".to_string(),
            };
            if room >= 6 {
                truncate(&full, room)
            } else {
                String::new()
            }
        }
    };
    (label, detail)
}

/// What the next Esc does in a dive ([`AppState::esc_does`]), long and short: it returns to
/// the orchestrator (`Esc: back`), or first closes a popup (`Esc: close`) or releases what is
/// selected (`Esc: release`), which a header saying "back" would misstate.
pub fn esc_words(state: &AppState) -> (&'static str, &'static str) {
    match state.esc_does() {
        EscDoes::Leave | EscDoes::Nothing => ("Esc returns to the orchestrator", "Esc: back"),
        EscDoes::Release => ("Esc: release", "Esc: release"),
        EscDoes::Close => ("Esc: close", "Esc: close"),
    }
}

/// What the main pane says while its transcript is empty: that it loads while its history is
/// still coming in, else what to do.
fn main_hint(state: &AppState) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let loading = state.transcript_catching_up();
    let text = match state.dived() {
        Some(t) if loading => vec![format!("Loading {}'s transcript…", t.name)],
        None if loading => vec!["Loading the conversation with the orchestrator…".to_string()],
        Some(t) => vec![format!(
            "Nothing from {} yet. What you type goes straight to it; Esc returns to the \
             orchestrator.",
            t.name
        )],
        None => vec![
            "This is your conversation with the orchestrator.".to_string(),
            "Tell it what you want done: it starts worker threads, steers them and reports \
             back. What you type always goes to it."
                .to_string(),
            "To talk to a worker directly, select its card (Tab) and press Enter twice (or \
             double-click it, or /open <thread>); Esc comes back here."
                .to_string(),
        ],
    };
    let mut out = Vec::new();
    for t in text {
        out.push(Line::from(Span::styled(t, dim)));
        out.push(Line::default());
    }
    out
}

/// The orchestrator's open attention items, at the top of its conversation, a confirm card of
/// the hub first ([`AppState::card_items`]): selectable like a card (its action popup answers
/// it, and dismisses what the hub raised).
fn render_orch_banner(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    orch: &str,
    panes: &mut Panes,
) {
    // A confirm card (the orchestrator's proposals from the user's message, or a delete
    // card) comes first.
    let title = match state.top_attention(orch) {
        Some(i) if i.proposal => confirm_title(state, i),
        _ => " the orchestrator needs you ".to_string(),
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )));
    block = if highlighted(state) == Some(orch) {
        block
            .border_type(BorderType::Thick)
            .border_style(Style::default().fg(Color::Yellow))
    } else {
        block
            .border_style(Style::default().fg(Color::Yellow))
            .title_bottom(Line::from(" Tab selects it ").right_aligned())
    };
    let inner = block.inner(area);
    panes.add(PaneId::Thread(orch.to_string()), inner);
    let attn = attention_rows(state, orch, usize::from(inner.width), false);
    let (rows, drawn) = card_rows(Vec::new(), attn, inner.height, inner.width);
    frame.render_widget(Paragraph::new(rows).block(block), area);
    record(panes, orch, drawn, inner);
}

/// The title of confirm card `item`: ` confirm deleting web, api ` for a delete card,
/// ` confirm what you asked ` for the orchestrator's proposals.
fn confirm_title(state: &AppState, item: &AttentionItem) -> String {
    match state.delete_card(&item.attn_id) {
        Some(names) => format!(" confirm deleting {} ", names.join(", ")),
        None => " confirm what you asked ".to_string(),
    }
}

/// The rows of confirm card `item` in its detail view, `width` cells wide: its badge and the
/// head of its summary, each action of the summary (split at ` · `) on rows of its own, a
/// blank row, then its options ([`option_rows`]), its note and how many more items wait
/// (`more`). Wrapped once and kept while the item, its text and the width stay the same
/// ([`WrapCache`]).
fn card_detail_rows(
    state: &AppState,
    item: &AttentionItem,
    more: usize,
    width: usize,
) -> Arc<DetailRows> {
    let item = shown_item(state, item);
    let item = item.as_ref();
    let key = WrapKey {
        attn_id: item.attn_id.clone(),
        revision: item.revision,
        width,
        tile: false,
        more,
        cut: None,
    };
    state
        .wraps
        .detail(key, item, || card_detail_rows_of(item, more, width))
}

/// [`card_detail_rows`] for `item`, wrapped now.
fn card_detail_rows_of(item: &AttentionItem, more: usize, width: usize) -> DetailRows {
    const HEAD: &str = "From your message:";
    let (head, rest) = match item.summary.strip_prefix(HEAD) {
        Some(rest) => (HEAD, rest.trim_start()),
        None => ("", item.summary.as_str()),
    };
    let mut rows = wrap_line(
        Line::from(vec![
            badge(item.reason),
            Span::raw(" "),
            Span::raw(head.to_string()),
        ]),
        width,
    );
    for action in rest.split(" · ").filter(|a| !a.trim().is_empty()) {
        // A span keeps the action's line breaks (`Line::from` a string would drop them).
        rows.extend(wrap_line(
            Line::from(Span::raw(format!("• {}", action.trim()))),
            width,
        ));
    }
    rows.push(Line::default());
    let mut tail = option_rows(&item.options, width);
    let options = rows.len()..rows.len() + tail.len();
    if let Some(note) = &item.note {
        tail.extend(wrap_line(
            Line::from(Span::styled(
                format!("note: {note}"),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::ITALIC),
            )),
            width,
        ));
    }
    if more > 0 {
        tail.extend(wrap_line(
            Line::from(Span::styled(
                format!("+{more} more"),
                Style::default().fg(Color::DarkGray),
            )),
            width,
        ));
    }
    rows.extend(tail);
    let exact = rows.iter().all(|l| l.width() <= width.max(1));
    DetailRows {
        rows,
        joins: Vec::new(),
        options,
        exact,
    }
}

/// A confirm card too long for card `orch` (the orchestrator's banner, or with no
/// orchestrator the hub's card, [`hub_detail`]), over the whole main pane `area`: every action
/// on rows of its own, then both options, scrolled with PgUp/PgDn and the wheel
/// ([`crate::app::CardView`]). It is selectable like that card. It counts as drawn in full
/// only when every row has been on screen (in this frame or an earlier one) and its option
/// rows are on screen now: the note and the "+N more" row after them need not be, so a note
/// taller than the pane never keeps the card from being answered.
fn render_card_detail(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    orch: &str,
    panes: &mut Panes,
) {
    let Some(item) = state
        .selected_request()
        .filter(|i| i.thread == orch)
        .or_else(|| state.top_attention(orch))
    else {
        return;
    };
    let request = !item.proposal
        && matches!(
            item.reason,
            AttentionReason::Permission | AttentionReason::Question
        );
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            if item.proposal {
                confirm_title(state, item)
            } else if request {
                format!(
                    " {} · request · Esc returns ",
                    state.registry.get(orch).map_or(orch, |t| t.name.as_str())
                )
            } else {
                // Another item the action popup had no room for ([`AppState::detail_card`]).
                let name = if orch == HUB_THREAD {
                    "twr asks"
                } else {
                    state.registry.get(orch).map_or(orch, |t| t.name.as_str())
                };
                format!(
                    " {name} · {} · Esc returns ",
                    reason_label(item.reason).to_lowercase()
                )
            },
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title_bottom(Line::from(" PgUp/PgDn or the wheel scroll it ").right_aligned());
    // What Esc returns from, a tap on its `✕` returns from too.
    let returns = !item.proposal;
    if returns {
        block = block.title(close_title());
    }
    // Selected: its card, or (the sidebar hidden) the request itself.
    let selected = highlighted(state) == Some(orch)
        || (request
            && state
                .selected_request()
                .is_some_and(|i| i.attn_id == item.attn_id));
    block = if selected {
        block
            .border_type(BorderType::Thick)
            .border_style(Style::default().fg(Color::Yellow))
    } else {
        block
            .border_style(Style::default().fg(Color::Yellow))
            .title_bottom(Line::from(" Tab selects it ").left_aligned())
    };
    let inner = block.inner(area);
    panes.add(PaneId::Thread(orch.to_string()), inner);
    if returns {
        close_row(panes, area, Target::Release);
    }
    let width = usize::from(inner.width);
    let more = if request {
        0
    } else {
        state.card_items(orch).len().saturating_sub(1)
    };
    let detail = if request {
        request_detail_rows(state, item, width)
    } else {
        card_detail_rows(state, item, more, width)
    };
    let (rows, options) = (&detail.rows, detail.options.clone());
    let height = usize::from(inner.height);
    let max_scroll = rows.len().saturating_sub(height);
    let cv = &state.view.card_view;
    let same = cv.attn_id == item.attn_id && cv.revision == item.revision;
    let start = if same { cv.scroll } else { 0 }.min(max_scroll);
    let end = (start + height).min(rows.len());
    let window = crate::select::CardWindow {
        thread: orch.to_string(),
        attn_id: item.attn_id.clone(),
        revision: item.revision,
        width: inner.width,
        rows: rows.len(),
        complete: detail.exact && state.incomplete(item).is_none(),
        start,
        end,
        max_scroll,
        page: inner.height,
        area: inner,
    };
    // Every row on screen now or before (in this layout), and the option rows on screen now.
    let before = cv.is_of(&window).then_some(&cv.seen);
    let all_seen =
        (0..rows.len()).all(|r| (start..end).contains(&r) || before.is_some_and(|s| s[r]));
    let options_shown = start <= options.start && options.end <= end;
    let full = all_seen && options_shown && detail.exact && state.incomplete(item).is_none();
    let drawn = full.then(|| (item.attn_id.clone(), item.revision));
    let shown: Vec<Line<'static>> = rows[start..end].to_vec();
    if !inner.is_empty() {
        panes.card = Some(window);
    }
    frame.render_widget(Paragraph::new(shown).block(block), area);
    record(panes, orch, drawn, inner);
}

/// Whether the main pane shows the hub's confirm card as a detail view
/// ([`render_card_detail`]): with no orchestrator its banner is not there, so the hub's card
/// in the stack hosts the confirm cards ([`AppState::card_items`]), and one it cut short as
/// this frame drew it (`panes`: the card is on screen, its top item a confirm card not drawn
/// in full) could never be answered there. The cards are drawn before the main pane.
fn hub_detail(state: &AppState, panes: &Panes) -> bool {
    state.orchestrator().is_none()
        && state.top_attention(HUB_THREAD).is_some_and(|i| i.proposal)
        && panes.get(&PaneId::Thread(HUB_THREAD.to_string())).is_some()
        && panes.drawn_on(HUB_THREAD).is_none()
}

/// The main pane's header band over `area` (one row): filled with where a typed line goes
/// ([`target_fill`]), its label bold ([`main_title`]), the details after it, the working
/// icon first while the thread shown works (`working`), and `(request selected, Esc follows)`
/// at the right while a request is selected and there is room for it (a view scrolled up
/// says so on the pane's bottom border, [`below_text`]).
fn render_band(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    working: Option<Span<'static>>,
    panes: &mut Panes,
) {
    let band = target_fill(state.dived().is_some());
    let width = usize::from(area.width);
    let mut spans = vec![Span::styled(" ", band)];
    // In a dive, the band takes you back: `‹ back` first, the whole row a control for a
    // finger.
    let back = state.dived().is_some() && width >= 20;
    if back {
        panes.target(area, Target::Back);
    }
    if let Some(cursor) = working {
        // On the terminal's own background, as on the cards: one cell after the edge.
        spans.push(cursor.patch_style(Style::default().bg(Color::Reset)));
        spans.push(Span::styled(" ", band));
        // Only where it shows: a header with a row and room for the cell.
        let cell = Rect::new(area.x + 1, area.y, 1, 1);
        if state.view.animate && area.height > 0 && on(area, cell) {
            panes.moves(cell);
        }
    }
    if back {
        spans.push(Span::styled("‹ back", band.add_modifier(Modifier::BOLD)));
        spans.push(Span::styled("  ", band));
    }
    if let Some(name) = state.instance_label() {
        spans.push(Span::styled(
            format!("[{}] ", truncate_cells(name, width / 3)),
            band.add_modifier(Modifier::BOLD),
        ));
    }
    let room = width.saturating_sub(Line::from(spans.clone()).width() + 1);
    let request_hint = "(request selected, Esc follows)";
    let wanted = if state.selected_request().is_some() {
        request_hint
    } else {
        ""
    };
    let suffix = if room >= wanted.len() + 16 {
        wanted
    } else {
        ""
    };
    let gap = if suffix.is_empty() { 0 } else { 2 };
    let (label, detail) = main_title(state, room - suffix.len() - gap);
    spans.push(Span::styled(label, band.add_modifier(Modifier::BOLD)));
    if !detail.is_empty() {
        spans.push(Span::styled(format!("  {detail}"), band));
    }
    if !suffix.is_empty() {
        let used = Line::from(spans.clone()).width();
        let pad = width.saturating_sub(used + suffix.len() + 1);
        spans.push(Span::styled(format!("{}{suffix}", " ".repeat(pad)), band));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(band), area);
}

/// Full request text and options from the hydrated attention stack.
fn request_detail_rows(state: &AppState, item: &AttentionItem, width: usize) -> Arc<DetailRows> {
    let item = shown_item(state, item);
    let item = item.as_ref();
    let cut = state.incomplete(item).map(|i| i.failed);
    let key = WrapKey {
        attn_id: item.attn_id.clone(),
        revision: item.revision,
        width,
        tile: false,
        more: 0,
        cut,
    };
    state.wraps.request(key, item, || {
        // A permission reads exactly as it runs: no space of it dropped by word wrapping.
        let summary = Line::from(Span::raw(item.summary.clone()));
        let mut rows = if item.reason == AttentionReason::Permission {
            wrap_exact(summary, width)
        } else {
            wrap_joined(summary, width)
        };
        if let Some(failed) = cut {
            rows.extend(wrap_joined(
                Line::from(if failed {
                    "(could not load the full request)"
                } else {
                    "(loading the full request…)"
                }),
                width,
            ));
        }
        // What it was raised in: enough to judge it without asking the orchestrator.
        if let Some(c) = &item.context {
            // `input`: all of a request whose summary shows only its start.
            let fields = [
                ("why", c.why.as_deref()),
                ("in", c.cwd.as_deref()),
                ("input", c.input.as_deref()),
                ("task", c.task.as_deref()),
                ("said", c.said.as_deref()),
            ];
            let fields: Vec<(&str, &str)> = fields
                .iter()
                .filter_map(|(k, v)| v.map(|v| (*k, v)))
                .collect();
            if !fields.is_empty() {
                rows.push((Line::default(), Join::Newline));
                rows.extend(context_rows(&fields, width));
            }
        }
        rows.push((Line::default(), Join::Newline));
        let option_lines = option_rows_joined(&item.options, width);
        let options = rows.len()..rows.len() + option_lines.len();
        rows.extend(option_lines);
        if let Some(note) = &item.note {
            rows.extend(wrap_joined(
                Line::from(Span::raw(format!("note: {note}"))),
                width,
            ));
        }
        let exact = rows.iter().all(|(r, _)| r.width() <= width.max(1));
        let (rows, joins) = rows.into_iter().unzip();
        DetailRows {
            rows,
            joins,
            options,
            exact,
        }
    })
}

fn request_rows(state: &AppState, item: &AttentionItem, width: u16) -> Vec<TranscriptRow> {
    let name = state
        .registry
        .get(&item.thread)
        .map_or(item.thread.as_str(), |t| t.name.as_str());
    let reason = if item.reason == AttentionReason::Permission {
        "permission"
    } else {
        "question"
    };
    let boxed = usize::from(width) >= BOX_MIN_WIDTH;
    let body_width = usize::from(width.max(1))
        .saturating_sub(if boxed { 4 } else { 0 })
        .max(1);
    let detail = request_detail_rows(state, item, body_width);
    let selected = state
        .selected_request()
        .is_some_and(|i| i.attn_id == item.attn_id);
    let color = if selected {
        Color::Yellow
    } else {
        thread_color(&item.thread)
    };
    let frame = Style::default().fg(color);
    let mut rows = thread_box(
        &format!("{name} · {reason}"),
        "",
        false,
        usize::from(width.max(1)),
        &Threads::default(),
    );
    for (line, ..) in &mut rows {
        for span in &mut line.spans {
            span.style = span.style.fg(color);
        }
    }
    for (line, join) in detail.rows.iter().zip(&detail.joins) {
        let mut line = line.clone();
        let edge = if boxed {
            let pad = " ".repeat(body_width.saturating_sub(line.width()));
            line.spans.insert(0, Span::styled("│ ", frame));
            line.spans.push(Span::styled(format!("{pad} │"), frame));
            Edge::Bars
        } else {
            Edge::None
        };
        rows.push((line, *join, None, edge));
    }
    if boxed {
        rows.push((
            Line::from(Span::styled(
                format!("╰{}╯", "─".repeat(usize::from(width) - 2)),
                frame,
            )),
            Join::Skip,
            None,
            Edge::Bottom,
        ));
    }
    for row in &mut rows {
        row.2 = Some(format!("request:{}", item.attn_id));
    }
    rows
}

fn request_tail(state: &AppState, id: &str, width: u16) -> usize {
    let entries = state.transcript.as_ref().map_or(&[][..], |t| t.visible().0);
    let collapse = Collapse {
        on: true,
        quiet_ends: true,
        all: state.view.expand_all,
        expanded: Some(&state.view.expanded),
    };
    let threads = Threads {
        boxes: true,
        state: Some(state),
    };
    let rows = transcript_view(entries, width, &collapse, &threads);
    let key = format!("request:{id}");
    rows.iter()
        .rposition(|r| r.2.as_deref() == Some(&key))
        .map_or(0, |end| rows.len() - end - 1)
}

pub(crate) fn request_scroll(state: &AppState, id: &str) -> usize {
    let width = state.view.panes.get(&PaneId::Transcript).map_or_else(
        || {
            state
                .view
                .panes
                .card
                .as_ref()
                .map_or(state.view.width, |p| p.area.width.saturating_add(2))
        },
        |p| p.area.width,
    );
    request_tail(state, id, width)
}

/// Conversation rows for block navigation, using the current pane's wrapping. No cursor
/// or scroll-follow policy is applied here: those remain the main renderer's responsibility.
#[cfg(test)]
pub(crate) fn block_rows(state: &AppState) -> Vec<TranscriptRow> {
    BlockView::new(state).rows
}

/// The main pane's conversation (the orchestrator's, or a dive's) as block navigation sees it,
/// wrapped at the main pane's width ([`block_rows`]): where its long blocks are, and the anchor that puts one of its
/// rows at the top of the pane ([`Anchor`]).
pub(crate) struct BlockView<'a> {
    state: &'a AppState,
    pub(crate) rows: Vec<TranscriptRow>,
    spans: Spans,
}

impl<'a> BlockView<'a> {
    pub(crate) fn new(state: &'a AppState) -> Self {
        let width = state
            .view
            .panes
            .get(&PaneId::Transcript)
            .map_or(state.view.width, |p| p.area.width);
        // As the main pane draws it ([`render_main`]): a worker's dive has no thread boxes and
        // no requests in it.
        let orchestrator = state.dived().is_none_or(ThreadView::is_orchestrator);
        let (rows, spans) = transcript_view_spans(
            state.transcript.as_ref().map_or(&[], |t| t.visible().0),
            width,
            &Collapse {
                on: true,
                quiet_ends: orchestrator,
                all: state.view.expand_all,
                expanded: Some(&state.view.expanded),
            },
            &Threads {
                boxes: orchestrator,
                state: Some(state),
            },
        );
        BlockView { state, rows, spans }
    }

    /// The steps Shift-↑/↓ go through with the main pane selected
    /// ([`crate::app::AppState`]'s `step_main`): its long blocks (their keys) and requests
    /// (`request:<attention id>`), in the order they are drawn, each with its first row.
    pub(crate) fn steps(&self) -> Vec<(String, usize)> {
        let mut steps: Vec<(String, usize)> = Vec::new();
        for (row, link) in self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.2.as_deref().map(|l| (i, l)))
        {
            let key = match linked_block(link) {
                Some(block) => block,
                None if link.starts_with("request:") => link,
                None => continue,
            };
            if steps.last().is_none_or(|(k, _)| k != key) {
                steps.push((key.to_string(), row));
            }
        }
        steps
    }

    /// The long blocks, in order (their keys).
    #[cfg(test)]
    pub(crate) fn blocks(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = Vec::new();
        for key in self
            .rows
            .iter()
            .filter_map(|r| r.2.as_deref().and_then(linked_block))
        {
            if keys.last() != Some(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// The rows of long block `key`, if it is there.
    pub(crate) fn block(&self, key: &str) -> Option<Range<usize>> {
        let of = |r: &TranscriptRow| r.2.as_deref().and_then(linked_block) == Some(key);
        let start = self.rows.iter().position(of)?;
        let n = self.rows[start..].iter().take_while(|r| of(r)).count();
        Some(start..start + n)
    }

    /// The anchor that puts row `row` at the top of the main pane ([`anchor_at`]).
    pub(crate) fn anchor(&self, row: usize) -> Option<Anchor> {
        let raise = |id: &str| raise_of(self.state, id);
        anchor_at(&self.rows, &self.spans, self.ids()?, &raise, row)
    }

    /// The row the main pane's top was last drawn at ([`Pane::top`]).
    pub(crate) fn top(&self) -> Option<usize> {
        let top = self
            .state
            .view
            .panes
            .get(&PaneId::Transcript)?
            .top
            .as_ref()?;
        let raise = |id: &str| raise_of(self.state, id);
        let (row, _) = anchor_row(&self.rows, &self.spans, self.ids()?, &raise, top);
        usize::try_from(row).ok()
    }

    fn ids(&self) -> Option<&'a [u64]> {
        let ids = self
            .state
            .transcript
            .as_ref()
            .map_or(&[][..], |t| t.visible().1);
        (ids.len() == self.spans.entries.len()).then_some(ids)
    }
}

/// The event that raised request `id`, as it is now ([`RequestCard::raise`]).
fn raise_of(state: &AppState, id: &str) -> Option<String> {
    state.attention.get(id).map(|i| i.event_id.clone())
}

/// The main pane: the orchestrator conversation with live request cards, or a worker dive.
fn render_main(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let dive = state.dived();
    // The thread the pane shows: its working icon leads the header while it works.
    let shown = dive.or_else(|| state.orchestrator());
    let works = shown.is_some_and(icon::is_working);
    let working = shown.and_then(|t| working_icon(state, t));
    let color = target_color(dive.is_some());
    let [band, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    render_band(frame, band, state, working, panes);
    // Where typed lines go: its thick border in the target's color. Selected (Shift-←), it is
    // drawn as a selected card is, and its bottom border says so.
    let mut block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(color));
    let selected = state.main_focused();
    if selected {
        block = block
            .border_type(BorderType::Thick)
            .border_style(Style::default().fg(Color::Yellow));
    }
    let mut inner = block.inner(body);
    frame.render_widget(block, body);
    let label = if selected {
        render_selected_label(frame, body)
    } else {
        0
    };
    // An item the action popup had no room for, read here, in a dive or not.
    if let Some(card) = state.detail_card() {
        render_card_detail(frame, inner, state, card, panes);
        return;
    }
    // The orchestrator's own cards (an error, a stall, a question) on top of its conversation.
    if dive.is_none() {
        if let Some(o) = state.orch_banner() {
            let width = usize::from(inner.width.saturating_sub(2));
            let n =
                attention_rows(state, &o, width, false).map_or(0, |a| a.core.len() + a.extra.len());
            let n = u16::try_from(n).unwrap_or(u16::MAX);
            // A confirm card too long for the banner takes the whole pane, and scrolls.
            let proposal = state.top_attention(&o).is_some_and(|i| i.proposal);
            if proposal && n.saturating_add(2) > inner.height.saturating_sub(3) {
                render_card_detail(frame, inner, state, &o, panes);
                return;
            }
            let h = n.saturating_add(2).min(inner.height.saturating_sub(3));
            if h >= 3 {
                let [banner, rest] =
                    Layout::vertical([Constraint::Length(h), Constraint::Min(0)]).areas(inner);
                render_orch_banner(frame, banner, state, &o, panes);
                inner = rest;
            }
        } else if hub_detail(state, panes) {
            render_card_detail(frame, inner, state, HUB_THREAD, panes);
            return;
        }
    }
    // Keep the banner visible and page long requests within the remaining conversation.
    if let Some(item) = state.selected_request() {
        let rows = request_rows(state, item, inner.width);
        if rows.len() >= usize::from(inner.height) {
            render_card_detail(frame, inner, state, &item.thread, panes);
            return;
        }
    }
    let mut pane = Pane::new(PaneId::Transcript, inner);
    // A replay being applied is not drawn half done (`Transcript::visible`).
    let (entries, ids) = state
        .transcript
        .as_ref()
        .map_or((&[][..], &[][..]), |t| t.visible());
    if entries.is_empty() && (dive.is_some() || state.inline_requests().is_empty()) {
        panes.push(pane);
        frame.render_widget(
            Paragraph::new(main_hint(state)).wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let orchestrator = dive.is_none_or(ThreadView::is_orchestrator);
    let collapse = Collapse {
        on: true,
        quiet_ends: orchestrator,
        all: state.view.expand_all,
        expanded: Some(&state.view.expanded),
    };
    let threads = Threads {
        boxes: orchestrator,
        state: Some(state),
    };
    let (mut rows, spans) = transcript_view_spans(entries, inner.width, &collapse, &threads);
    if entries.is_empty() && state.selected_request().is_none() {
        rows.extend(
            main_hint(state)
                .into_iter()
                .map(|line| (line, Join::Newline, None, Edge::None)),
        );
    }
    let height = usize::from(inner.height);
    // (Entries put there without their ids are not anchored on.)
    let anchors = ids.len() == spans.entries.len();
    let raise = |id: &str| raise_of(state, id);
    // Scrolled up, the view stays on the place it was anchored on: the row there, before it
    // is kept to the rows there are. A view at the bottom (or anchored on rows now there)
    // follows it.
    let anchored = state
        .view
        .anchor
        .as_ref()
        .filter(|_| state.view.scroll > 0 && anchors)
        .map(|a| anchor_row(&rows, &spans, ids, &raise, a));
    let found = anchored.is_some_and(|(_, found)| found);
    let anchored = anchored.map(|(row, _)| row);
    let top = |row: isize| usize::try_from(row).unwrap_or(0);
    let scrolled = match anchored {
        Some(row) => top(row) < rows.len().saturating_sub(height),
        None => state.view.scroll.min(rows.len().saturating_sub(height)) > 0,
    };
    // While the thread works, the cursor waits where its next text will appear (never in a
    // pane with no cell to show it).
    let drawn = rows.len();
    let cursor = (works && !inner.is_empty())
        .then(|| place_cursor(&mut rows, entries, inner.width, state, scrolled))
        .flatten();
    let follow_rows = rows.len() - drawn;
    let requests = if dive.is_none() {
        state
            .inline_requests()
            .into_iter()
            .filter_map(|item| {
                let key = format!("request:{}", item.attn_id);
                let from = rows.iter().position(|r| r.2.as_deref() == Some(&key))?;
                let end = rows.iter().rposition(|r| r.2.as_deref() == Some(&key))? + 1;
                Some((item, from..end))
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let max_scroll = rows.len().saturating_sub(height);
    let scroll = match anchored {
        _ if !scrolled => 0,
        Some(row) => max_scroll - top(row).min(max_scroll),
        None => state.view.scroll,
    };
    // New replies must not scroll a selected request away. Esc or an explicit transcript
    // scroll releases the selection; until then its options stay in view.
    let scroll = requests
        .iter()
        .find(|(item, _)| state.view.request.as_deref() == Some(item.attn_id.as_str()))
        .map_or(scroll, |(_, range)| {
            let scroll = scroll.min(max_scroll);
            let start = max_scroll - scroll;
            if range.start < start {
                // Move just far enough to show the request, leaving room for its reply.
                max_scroll.saturating_sub(range.start)
            } else if range.end > start + height {
                rows.len().saturating_sub(range.end)
            } else {
                scroll
            }
        })
        .min(max_scroll);
    let start = rows.len().saturating_sub(height + scroll);
    let end = (start + height).min(rows.len());
    for (item, range) in requests {
        if start <= range.start
            && range.end <= end
            && state.incomplete(item).is_none()
            && rows[range.clone()]
                .iter()
                .all(|r| r.0.width() <= usize::from(inner.width))
        {
            panes.drawn(crate::select::DrawnItem {
                thread: item.thread.clone(),
                attn_id: item.attn_id.clone(),
                revision: item.revision,
                area: Rect::new(
                    inner.x,
                    inner.y + (range.start - start) as u16,
                    inner.width,
                    range.len() as u16,
                ),
            });
        }
    }
    let mut lines = Vec::with_capacity(end - start);
    for (y, (line, join, link, edge)) in (inner.y..).zip(rows[start..end].iter().cloned()) {
        lines.push(line);
        // A row all drawing adds nothing to a copy, though a long block's footer continues
        // the line it cuts for places ([`long_rows`]).
        pane.joins.push(if edge == Edge::Bottom {
            Join::Skip
        } else {
            join
        });
        pane.links.push(link);
        // A thread box's frame is drawn, not copied.
        pane.skip.extend(frame_cells(edge, inner.x, y, inner.width));
    }
    // On screen, its cell is not the transcript's text, and a moving one keeps the clock:
    // only a cell inside the pane and the buffer.
    if let Some((row, col)) = cursor.filter(|(row, _)| (start..end).contains(row)) {
        let y = inner
            .y
            .saturating_add(u16::try_from(row - start).unwrap_or(u16::MAX));
        let x = inner
            .x
            .saturating_add(u16::try_from(col).unwrap_or(u16::MAX));
        let cell = Rect::new(x, y, 1, 1);
        if on(inner, cell) && on(frame.area(), cell) {
            pane.skip.push(cell);
            if state.view.animate {
                panes.moves(cell);
            }
        }
    }
    pane.max_scroll = max_scroll;
    pane.scroll = scroll;
    pane.follow_rows = follow_rows;
    pane.block = state.view.block.clone().filter(|key| {
        rows.iter()
            .any(|r| r.2.as_deref().and_then(linked_block) == Some(key))
    });
    if anchors {
        pane.top = anchor_at(&rows, &spans, ids, &raise, start);
    }
    // The anchor drawn right there, in its own card or entry, is kept. One moved by keys,
    // kept to the rows there are, overridden by a selected request, or standing for a card
    // or an entry gone (a request raised again under its id must not pull the view) is taken
    // again from the row drawn.
    // (Kept on a request's card, it stands for the entry the card is in now, should the card
    // go; kept on an entry, for that entry, though it shares a folded group's marker row.)
    pane.anchor = match (&state.view.anchor, anchored) {
        (Some(a), Some(row)) if found && a.rows == 0 && usize::try_from(row) == Ok(start) => {
            let entry = match a.request {
                Some(_) => pane.top.as_ref().and_then(|t| t.entry),
                None => a.entry,
            };
            Some(Anchor { entry, ..a.clone() })
        }
        _ => pane.top.clone(),
    };
    panes.push(pane);
    frame.render_widget(Paragraph::new(lines), inner);
    // Scrolled up: how much is below, on the bottom border, whose row a tap takes back to
    // the bottom (End).
    if scroll > 0 {
        render_below(frame, body, rows.len() - end, state, label);
        if body.height > 0 {
            let row = Rect::new(body.x, body.bottom() - 1, body.width, 1);
            panes.target(row, Target::Follow);
        }
    }
}

/// What the main pane's bottom border says while it is selected ([`AppState::main_focused`]),
/// the longest that fits in `width` cells.
pub fn selected_text(width: usize) -> String {
    [
        " selected · Shift-↑↓ blocks · Shift-→ cards ",
        " selected · Shift-→ cards ",
        " selected ",
    ]
    .into_iter()
    .find(|t| crate::edit::width(t) <= width)
    .unwrap_or_default()
    .to_string()
}

/// [`selected_text`] at the left of the bottom border of `body` (the main pane's block), in
/// at most half of it, so the note on what is below ([`render_below`]) keeps the rest.
/// Returns the cells it takes.
fn render_selected_label(frame: &mut Frame, body: Rect) -> u16 {
    if body.height == 0 || body.width < 4 {
        return 0;
    }
    let text = selected_text(usize::from((body.width - 4) / 2));
    let w = u16::try_from(crate::edit::width(&text)).unwrap_or(0);
    let area = Rect::new(body.x + 2, body.y + body.height - 1, w, 1);
    let style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
    w
}

/// The characters of a transcript row's text, each grapheme cluster one: not its whitespace,
/// nor what is drawn around the text ([`Edge`]: a gutter's marker or indentation, a box's
/// frame).
fn text_chars(row: &TranscriptRow) -> usize {
    let (line, _, _, edge) = row;
    // Cluster by cluster, as rows are wrapped ([`clusters`]).
    let text: Vec<&str> = line
        .spans
        .iter()
        .flat_map(|s| clusters(&s.content).into_iter().map(|(_, g)| g))
        .collect();
    let width = text.iter().map(|g| cluster_width(g)).sum::<usize>();
    let cells = match *edge {
        Edge::None => 0..width,
        Edge::Gutter(n) => usize::from(n)..width,
        Edge::Bars => 2..width.saturating_sub(2),
        Edge::BarsIndent(n) => 2 + usize::from(n)..width.saturating_sub(2),
        Edge::Top(_) | Edge::Bottom => return 0,
    };
    let mut at = 0;
    let mut n = 0;
    for g in text {
        if cells.contains(&at) && !g.chars().all(char::is_whitespace) {
            n += 1;
        }
        at += cluster_width(g);
    }
    n
}

/// Where in their entry (or request card) `rows` are, from its first row ([`Anchor`]): the
/// line each is on, the characters of that line's text before it, and the rows of that line
/// before it starting at the same character.
fn places(rows: &[TranscriptRow]) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
    let (mut line, mut chars, mut repeat) = (0, 0, 0);
    rows.iter().enumerate().map(move |(k, row)| {
        if k > 0 && !matches!(row.1, Join::Space | Join::Glue) {
            line += 1;
            chars = 0;
            repeat = 0;
        }
        let at = (line, chars, repeat);
        let n = text_chars(row);
        chars += n;
        repeat = if n == 0 { repeat + 1 } else { 0 };
        at
    })
}

/// The row of `rows` (an entry's or a card's) at `anchor`'s place: the last at or before it
/// (the last row when the text got shorter), counted from the first.
fn place_row(rows: &[TranscriptRow], anchor: &Anchor) -> usize {
    let place = (anchor.line, anchor.chars, anchor.repeat);
    places(rows)
        .take_while(|&p| p <= place)
        .count()
        .saturating_sub(1)
}

/// The section of entry `k` (its rows in `spans`) that its row `row` is in, from 0, and the
/// section's first row.
fn section_at(spans: &Spans, k: usize, row: usize) -> (usize, usize) {
    let lo = spans
        .sections
        .partition_point(|&b| b <= spans.entries[k].start);
    let hi = spans.sections.partition_point(|&b| b <= row).max(lo);
    let from = spans.sections[lo..hi]
        .last()
        .copied()
        .unwrap_or(spans.entries[k].start);
    (hi - lo, from)
}

/// The rows of section `n` of entry `k` (its rows in `spans`); `None` when it has no such
/// section any more.
fn section_rows(spans: &Spans, k: usize, n: usize) -> Option<Range<usize>> {
    let entry = &spans.entries[k];
    let lo = spans.sections.partition_point(|&b| b <= entry.start);
    let hi = spans.sections.partition_point(|&b| b < entry.end).max(lo);
    let starts = &spans.sections[lo..hi];
    let start = match n {
        0 => entry.start,
        n => *starts.get(n - 1)?,
    };
    Some(start..starts.get(n).copied().unwrap_or(entry.end))
}

/// The entry whose rows (`spans`, [`transcript_view_spans`]) hold row `row`, if any.
fn entry_at(spans: &[Range<usize>], row: usize) -> Option<usize> {
    let above = spans.partition_point(|s| s.start <= row);
    spans[..above]
        .iter()
        .rposition(|s| !s.is_empty())
        .filter(|&k| spans[k].contains(&row))
}

/// The request whose card ([`request_rows`]) `row` is a row of.
fn card_of(row: &TranscriptRow) -> Option<&str> {
    row.2.as_deref()?.strip_prefix("request:")
}

/// The rows of request `id`'s card, if it is drawn.
fn card_span(rows: &[TranscriptRow], id: &str) -> Option<Range<usize>> {
    let start = rows.iter().position(|r| card_of(r) == Some(id))?;
    let n = rows[start..]
        .iter()
        .take_while(|r| card_of(r) == Some(id))
        .count();
    Some(start..start + n)
}

/// The anchor of the transcript's row `row` ([`Anchor`]), from its `rows`, the rows each
/// entry drew (`spans`, [`transcript_view_spans`]), the entries' `ids` and the raise of each
/// request drawn (`raise`, [`RequestCard::raise`]): its place in the request card or the
/// entry that drew it. A row of neither (a blank row between them, the working cursor's) is
/// rows above the next one that is, or rows below the last. `None` with no such row.
pub fn anchor_at(
    rows: &[TranscriptRow],
    spans: &Spans,
    ids: &[u64],
    raise: &dyn Fn(&str) -> Option<String>,
    row: usize,
) -> Option<Anchor> {
    let entry = entry_at(&spans.entries, row);
    let card = card_of(rows.get(row)?).and_then(|id| Some((id, raise(id)?)));
    let (from, request, section) = match (card, entry) {
        (Some((id, raise)), _) => {
            let from = (0..row)
                .rev()
                .take_while(|&r| card_of(&rows[r]) == Some(id))
                .last()
                .unwrap_or(row);
            let card = RequestCard {
                attn_id: id.to_string(),
                raise,
            };
            (from, Some(card), 0)
        }
        (None, Some(k)) => {
            let (section, from) = section_at(spans, k, row);
            (from, None, section)
        }
        (None, None) => {
            let placed =
                |r: &usize| card_of(&rows[*r]).is_some() || entry_at(&spans.entries, *r).is_some();
            let near = (row + 1..rows.len())
                .find(placed)
                .or_else(|| (0..row).rev().find(placed))?;
            let a = anchor_at(rows, spans, ids, raise, near)?;
            return Some(Anchor {
                rows: isize::try_from(row).unwrap_or(isize::MAX)
                    - isize::try_from(near).unwrap_or(isize::MAX),
                ..a
            });
        }
    };
    let (line, chars, repeat) = places(&rows[from..=row]).last()?;
    Some(Anchor {
        entry: entry.map(|k| ids[k]),
        request,
        section,
        line,
        chars,
        repeat,
        rows: 0,
    })
}

/// The row `anchor` puts at the top of the view ([`anchor_at`]), before it is kept to the
/// rows there are, and whether its card or entry is there: the row of its request's card (of
/// the same raise) or of its entry at its place ([`place_row`]: the last row when the text
/// got shorter, the group's marker row when it was folded), then its rows on. A card no longer
/// drawn stands at its entry's first row (above the entries: at the top); an entry removed
/// since, where the entry after it starts.
pub fn anchor_row(
    rows: &[TranscriptRow],
    spans: &Spans,
    ids: &[u64],
    raise: &dyn Fn(&str) -> Option<String>,
    anchor: &Anchor,
) -> (isize, bool) {
    let card = anchor
        .request
        .as_ref()
        .filter(|c| raise(&c.attn_id).as_deref() == Some(c.raise.as_str()))
        .and_then(|c| card_span(rows, &c.attn_id));
    let (at, found) = match (card, anchor.entry.map(|e| ids.binary_search(&e))) {
        (Some(card), _) => (card.start + place_row(&rows[card], anchor), true),
        (None, Some(Ok(k))) if anchor.request.is_some() => (spans.entries[k].start, false),
        (None, Some(Ok(k))) => match section_rows(spans, k, anchor.section) {
            Some(section) => (section.start + place_row(&rows[section], anchor), true),
            // A section it no longer has: the entry's last row.
            None => {
                let entry = &spans.entries[k];
                (entry.end.saturating_sub(1).max(entry.start), true)
            }
        },
        (None, Some(Err(k))) => (spans.entries.get(k).map_or(rows.len(), |s| s.start), false),
        (None, None) => (0, false),
    };
    let row = isize::try_from(at)
        .unwrap_or(isize::MAX)
        .saturating_add(anchor.rows);
    (row, found)
}

/// What the main pane's bottom border says while the view is scrolled up: how many rows are
/// below it and the key that jumps there and follows again (`End`; `Ctrl-End` while a line
/// with text is being typed, where `End` moves the cursor). Shortened to fit a narrow pane, from
/// `↓ 12 lines below · End to follow` down to `↓ 12`.
pub fn below_text(below: usize, end_key: &str, width: usize) -> String {
    let lines = if below == 1 { "line" } else { "lines" };
    [
        format!(" ↓ {below} {lines} below · {end_key} to follow "),
        format!(" ↓ {below} below · {end_key} "),
        format!(" ↓ {below} more "),
        format!(" ↓ {below} "),
        " ↓ ".to_string(),
    ]
    .into_iter()
    .find(|t| crate::edit::width(t) <= width)
    .unwrap_or_default()
}

/// [`below_text`] on the bottom border of `body` (the main pane's block), at its right, past
/// the `label` cells at its left ([`render_selected_label`]).
fn render_below(frame: &mut Frame, body: Rect, below: usize, state: &AppState, label: u16) {
    if body.height == 0 || body.width < 4 + label {
        return;
    }
    // As the key takes it ([`AppState::handle_key`]): `End` edits a line that has text.
    let end_key = if state.view.input.is_empty() {
        "End"
    } else {
        "Ctrl-End"
    };
    // Inside the corners, a cell of border on each side (and one after a label).
    let room = usize::from(body.width - 4 - label).saturating_sub(usize::from(label.min(1)));
    let text = below_text(below, end_key, room);
    let w = u16::try_from(crate::edit::width(&text)).unwrap_or(0);
    let area = Rect::new(body.x + body.width - 2 - w, body.y + body.height - 1, w, 1);
    let style = target_fill(state.dived().is_some()).add_modifier(Modifier::BOLD);
    frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
}

/// Puts the working cursor ([`icon::cursor`]) into the transcript's `rows` (`width` cells
/// wide, its view `scrolled` up or not): at the end of the reply streaming in (the last entry is
/// open text) when its last row has room, else on a row of its own below the last one, but
/// only while the view follows the tail (a row added below a view scrolled up would move
/// it). In a thread box still streaming in, the cursor stays inside the box: after its text
/// on the box's last row, else on a new row of the box. Returns where it went (row, column),
/// if it did.
fn place_cursor(
    rows: &mut Vec<TranscriptRow>,
    entries: &[Entry],
    width: u16,
    state: &AppState,
    scrolled: bool,
) -> Option<(usize, usize)> {
    let cursor = icon::cursor(state.view.animate.then_some(state.view.frame));
    let streaming = matches!(entries.last(), Some(Entry::Text { open: true, .. }));
    // A long block's footer stays at the same height when streaming finishes. The header
    // already shows that the orchestrator is working; do not add a cursor below the preview.
    if rows
        .last()
        .and_then(|r| r.2.as_deref())
        .is_some_and(|l| l.starts_with("toggle:block:"))
    {
        return None;
    }
    // The last row is in a box still streaming in: its top border or a row of its text.
    let edge = rows.last().filter(|_| streaming).map(|r| r.3);
    let boxed = matches!(edge, Some(Edge::Bars | Edge::BarsIndent(_) | Edge::Top(_)));
    if let Some(last) = rows.last_mut().filter(|_| streaming) {
        match edge {
            Some(Edge::Bars | Edge::BarsIndent(_)) => {
                // `│ `, the text's spans, then the padding and ` │` ([`thread_box`]).
                let n = last.0.spans.len();
                let used: usize = last.0.spans[1..n.saturating_sub(1).max(1)]
                    .iter()
                    .map(Span::width)
                    .sum();
                let inner = usize::from(width).saturating_sub(4);
                if let Some(pad) = last.0.spans.last_mut().filter(|_| used < inner && n >= 2) {
                    let rest: String = pad.content.chars().skip(1).collect();
                    pad.content = rest.into();
                    last.0.spans.insert(n - 1, cursor);
                    return Some((rows.len() - 1, used + 2));
                }
            }
            Some(Edge::Top(_)) => {}
            _ => {
                // Cells as drawn (a control character takes none).
                let col: usize = last
                    .0
                    .spans
                    .iter()
                    .map(|s| crate::edit::width(&s.content))
                    .sum();
                if col < usize::from(width) {
                    last.0.spans.push(cursor);
                    return Some((rows.len() - 1, col));
                }
            }
        }
    }
    if scrolled {
        return None;
    }
    if boxed {
        let frame = rows.last().map_or(Style::default(), |r| r.0.spans[0].style);
        let pad = " ".repeat(usize::from(width).saturating_sub(5));
        let line = Line::from(vec![
            Span::styled("│ ", frame),
            cursor,
            Span::styled(format!("{pad} │"), frame),
        ]);
        rows.push((line, Join::Newline, None, Edge::Bars));
        return Some((rows.len() - 1, 2));
    }
    // On a row of its own: after a blank row like any block, or right under a reply still
    // streaming in.
    if !streaming && rows.last().is_some_and(|r| r.0.width() > 0) {
        rows.push((Line::default(), Join::Newline, None, Edge::None));
    }
    rows.push((Line::from(cursor), Join::Newline, None, Edge::None));
    Some((rows.len() - 1, 0))
}

/// Rows of text the input box shows at most on a screen `height` rows tall: a third of it,
/// at least one, at most [`INPUT_MAX_ROWS`].
pub fn input_cap(height: u16) -> usize {
    usize::from(height / 3).clamp(1, INPUT_MAX_ROWS)
}

/// Height of the input box on a screen `area`: its borders and the rows its text wraps into
/// at the box's width, at most [`input_cap`] of them. The panes above it give up the rows.
fn input_height(state: &AppState, area: Rect) -> u16 {
    let cols = usize::from(area.width.saturating_sub(2));
    let rows = state.view.input.height(cols, input_cap(area.height));
    u16::try_from(rows).unwrap_or(1).saturating_add(2)
}

/// The input box: the line being typed, wrapped at its width and scrolled to the cursor's row
/// ([`LineInput::view`]), under a title that says where it goes in that target's color and
/// label (`→ orchestrator`, `→ <thread>` in a dive), filled like the main pane's header.
fn render_input(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let dive = state.dived();
    let color = target_color(dive.is_some());
    let to = match dive {
        Some(t) => format!(" → {} ", t.name),
        None => " → orchestrator ".to_string(),
    };
    let label = target_fill(dive.is_some()).add_modifier(Modifier::BOLD);
    // In a dive, what the next Esc does ([`esc_words`]).
    let back = match (state.view.dive.is_some(), state.esc_does()) {
        (false, _) => "",
        (true, EscDoes::Release) => " · Esc: release",
        (true, EscDoes::Close) => " · Esc: close",
        (true, _) => " · Esc: back to the orchestrator",
    };
    let dim = Style::default().fg(Color::DarkGray);
    let style = Style::default().fg(color);
    // While the double Esc is armed, what the second Esc does, in place of what Enter does.
    let says = state
        .unsend_hint()
        .unwrap_or_else(|| format!(" Enter sends{back} "));
    let title = vec![Span::styled(to, label), Span::styled(says, style)];
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Line::from(title));
    // While the last message can be taken back, the border's row takes it back: a tap arms
    // the second, as the first Esc does (the title then says so in place of this).
    if state.take_back_offered() {
        if state.unsend_hint().is_none() {
            block = block.title(
                Line::from(Span::styled(
                    " ↶ take back ",
                    style.add_modifier(Modifier::BOLD),
                ))
                .right_aligned(),
            );
        }
        panes.target(
            Rect::new(area.x, area.y, area.width, area.height.min(1)),
            Target::TakeBack,
        );
    }
    let inner = block.inner(area);
    let line = &state.view.input;
    let view = line.view(usize::from(inner.width), usize::from(inner.height));
    let mut pane = Pane::new(PaneId::Input, inner);
    let text = if line.is_empty() {
        // Who a typed line goes to, until something is typed.
        let who = match dive {
            Some(t) => format!("message {} directly…", t.name),
            None => "message the orchestrator…".to_string(),
        };
        vec![Line::from(Span::styled(who, dim))]
    } else {
        // A row goes on from the one above: after its line break, after the space it wrapped
        // at (copied as one space), or inside a word cut at the edge.
        let mut prev: Option<&str> = None;
        for row in &view.rows {
            pane.joins.push(match prev {
                None => Join::Newline,
                Some(p) if p.ends_with('\n') => Join::Newline,
                Some(p) if p.ends_with(char::is_whitespace) => Join::Space,
                Some(_) => Join::Glue,
            });
            prev = Some(row);
        }
        view.rows
            .iter()
            .map(|r| Line::from(r.strip_suffix('\n').unwrap_or(r)))
            .collect()
    };
    panes.push(pane);
    frame.render_widget(Paragraph::new(text).block(block), area);
    // The action popup takes the keys while it is open: no cursor says they type.
    if state.action_popup().is_none() && inner.width > 0 && inner.height > 0 {
        let (row, col) = view.cursor;
        let at = |n: usize, max: u16| u16::try_from(n).unwrap_or(u16::MAX).min(max - 1);
        frame.set_cursor_position((
            inner.x + at(col, inner.width),
            inner.y + at(row, inner.height),
        ));
    }
}

/// The popup above the input line: the commands (or an argument's values, or the threads an
/// `@` mention can name) matching what is typed, the selected one highlighted. Returns where
/// it was drawn.
fn render_popup(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    panes: &mut Panes,
) -> Option<Rect> {
    let s = state.popup()?;
    let input = panes.get(&PaneId::Input).map(|p| p.area)?;
    // The input box's top border is the row above its text.
    let bottom = input.y.saturating_sub(1);
    let rows = s.items.len().min(POPUP_ROWS);
    let height = (rows as u16 + 2).min(bottom.saturating_sub(area.y));
    if height < 3 {
        return None;
    }
    let fit = usize::from(height - 2);
    let label_w = s
        .items
        .iter()
        .map(|i| i.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(44);
    let detail_w = s
        .items
        .iter()
        .map(|i| i.detail.chars().count())
        .max()
        .unwrap_or(0);
    let x = input.x.saturating_sub(1);
    let max_w = frame.area().width.saturating_sub(x);
    let width = (u16::try_from(label_w + detail_w + 6).unwrap_or(u16::MAX)).min(max_w);
    let rect = Rect::new(x, bottom - height, width, height);
    let sel = state.popup_selected(s.items.len());
    let first = table_first_row(sel, fit);
    let inner_w = usize::from(width.saturating_sub(2));
    let lines: Vec<Line> = s
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(fit)
        .map(|(i, item)| {
            let label = format!(" {:<label_w$} ", truncate(&item.label, label_w));
            let detail = truncate(&item.detail, inner_w.saturating_sub(label_w + 3));
            let mut label_style = Style::default().fg(Color::Cyan);
            let mut detail_style = Style::default().fg(Color::DarkGray);
            if i == sel {
                label_style = label_style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
                detail_style = Style::default().add_modifier(Modifier::REVERSED);
            }
            Line::from(vec![
                Span::styled(label, label_style),
                Span::styled(format!(" {detail}"), detail_style),
            ])
        })
        .collect();
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(match (s.command, s.mention) {
            (true, _) => " commands ",
            (_, true) => " threads ",
            _ => " values ",
        })
        .title(close_title());
    if s.items.len() > fit {
        block = block
            .title_bottom(Line::from(format!(" {}/{} ", sel + 1, s.items.len())).right_aligned());
    }
    let inner = block.inner(rect);
    panes.cover(Some(rect));
    panes.add(PaneId::Popup, inner);
    // A tap on a row completes it, as Tab does; on the top border, closes it.
    close_row(panes, rect, Target::Close);
    for k in 0..usize::from(inner.height).min(s.items.len().saturating_sub(first)) {
        let row = Rect::new(inner.x, inner.y + k as u16, inner.width, 1);
        panes.target(row, Target::Suggestion(s.items[first + k].value.clone()));
    }
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
    Some(rect)
}

/// The action popup ([`crate::app::ActionPopup`]), where the `/` popup is: above the input
/// box, as wide as it. Its title names what it acts on; under it, the selection's attention
/// item ([`popup_item_rows`]: its badge and summary, a request's context, a confirm card's
/// actions on rows of their own), then its rows: each option, numbered, then what else can be
/// done (talk to it, dismiss, pin, stop, park or resume, attach), the highlighted one reversed
/// and those its filter leaves out dimmed. Drawn whole, it covers what is under it and draws
/// its item in full ([`DrawnItem`]): what its answers act on. An item it has no room for is
/// drawn in full only when it had been read already ([`crate::app::ActionPopup::read`]): a
/// line says so over its options; else the popup says the item is open in the main pane and
/// lists no option, and the main pane opens the item's detail view ([`Panes::long_item`]).
/// Once that view was read in full, an option taller than the popup's room is listed on a
/// row by its number and first line. Returns where it was drawn.
fn render_action_popup(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    panes: &mut Panes,
) -> Option<Rect> {
    use crate::app::{ActionKind, ActionTarget};
    let popup = state.action_popup()?;
    let input = panes.get(&PaneId::Input).map(|p| p.area)?;
    // The input box's top border is the row above its text.
    let bottom = input.y.saturating_sub(1);
    let room = bottom.saturating_sub(area.y);
    let x = input.x.saturating_sub(1);
    let width = input
        .width
        .saturating_add(2)
        .min(frame.area().width.saturating_sub(x));
    if width < 3 || room < 3 {
        return None;
    }
    let inner_w = usize::from(width - 2);
    let fit = usize::from(room - 2);
    let dim = Style::default().fg(Color::DarkGray);
    let rows = state.action_rows(&popup.target);
    let matches = state.action_matches(popup, &rows);
    let item = state.selected_attention();
    let card = match &popup.target {
        ActionTarget::Card(id) => id.clone(),
        ActionTarget::Request(_) => item.map(|i| i.thread.clone()).unwrap_or_default(),
    };
    // Each row on lines of its own, a cell in from the border: its label, or `label` in its
    // place.
    let row_lines_as = |i: usize, label: &str| -> Vec<Line<'static>> {
        let row = &rows[i];
        let kept = matches.contains(&i);
        let selected = kept && popup.sel == Some(row.kind);
        // In a span, which keeps an option's line breaks (`Line::from` a string would drop
        // them), each line then on rows of its own, as [`option_rows`] draws them.
        let mut lines = wrap_line(Line::from(Span::raw(format!(" {label}"))), inner_w.max(1));
        for (k, line) in lines.iter_mut().enumerate() {
            let pad = inner_w.saturating_sub(line.width());
            line.spans.push(Span::raw(" ".repeat(pad)));
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else if !kept {
                dim
            } else {
                Style::default()
            };
            for span in &mut line.spans {
                span.style = style;
            }
            // An answer's number in the color the cards give it.
            if k == 0 && kept && !selected && matches!(row.kind, ActionKind::Answer(n) if n <= 9) {
                let text = line.spans[0].content.to_string();
                let number: String = text.chars().take(2).collect();
                let rest: String = text.chars().skip(2).collect();
                line.spans[0] = Span::raw(rest);
                line.spans.insert(
                    0,
                    Span::styled(
                        number,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                );
            }
        }
        lines
    };
    let row_lines = |i: usize| row_lines_as(i, &rows[i].label);
    // The item's rows a cell in from the border, as the rows under them.
    let head_w = inner_w.saturating_sub(1).max(1);
    let (mut head, mut exact) = match item {
        Some(i) => popup_item_rows(state, &card, i, head_w),
        None => (popup_report_rows(state, &popup.target, head_w), true),
    };
    let all: Vec<usize> = (0..rows.len()).collect();
    let height_of = |head: &[Line<'static>], shown: &[usize]| {
        let rule = usize::from(!head.is_empty() && !shown.is_empty());
        head.len() + rule + shown.iter().map(|&i| row_lines(i).len()).sum::<usize>()
    };
    // A report (no item) takes the room the rows leave (with a filter, those it keeps, maybe
    // none: the rest go first when they do not all fit); what does not fit ends in `…`.
    if item.is_none() {
        let kept = if popup.filter.is_empty() {
            all.clone()
        } else {
            matches.clone()
        };
        let room = fit.saturating_sub(height_of(&[], &kept) + usize::from(!kept.is_empty()));
        if head.len() > room {
            head.truncate(room);
            if let Some(last) = head.last_mut() {
                *last = ellipsized(std::mem::take(last), head_w);
            }
        }
    }
    // A report's popup whose filter matches no row shows the report alone: no row would be
    // drawn under it, so none takes its room. (An item's keeps its options: it counts as
    // drawn only with them.)
    let mut shown = if item.is_none() && !popup.filter.is_empty() && matches.is_empty() {
        Vec::new()
    } else {
        all.clone()
    };
    let mut whole = true;
    // The item's detail view has had every row of its whole text on screen for this revision
    // ([`crate::app::CardView::read_in_full`]): read in full, whatever the popup has room for
    // now. A capped preview read to its end is not, nor is the full text fetched after it.
    let detail_read = item.is_some_and(|i| {
        state.incomplete(i).is_none() && state.view.card_view.read_in_full(&i.attn_id, i.revision)
    });
    if let Some(i) = item.filter(|_| height_of(&head, &all) > fit) {
        let read = detail_read || popup.read.as_ref() == Some(&(i.attn_id.clone(), i.revision));
        let note = if read {
            "read in full in the main pane: answer it here"
        } else {
            // Not room for it: no option is listed, and the main pane shows it to be read.
            shown.retain(|&k| !matches!(rows[k].kind, ActionKind::Answer(_)));
            whole = false;
            panes.long_item = Some(card.clone());
            "too long to show here: it is open in the main pane. Esc, read it to its end \
             (PgUp/PgDn), then Enter here again to answer it"
        };
        head = wrap_line(Line::from(Span::styled(note, dim)), head_w);
        exact = true;
    }
    let mut lines: Vec<Line<'static>> = head
        .iter()
        .map(|l| {
            let mut l = l.clone();
            l.spans.insert(0, Span::raw(" "));
            l
        })
        .collect();
    // Not even a row's room under the item's text (a short screen): its end is cut, and it is
    // not drawn in full.
    if !shown.is_empty() && lines.len() + usize::from(!lines.is_empty()) >= fit {
        lines.truncate(fit.saturating_sub(2));
        whole = false;
        // A report cut short says so.
        if let Some(last) = lines.last_mut().filter(|_| item.is_none()) {
            *last = ellipsized(std::mem::take(last), inner_w);
        }
    }
    let rule = usize::from(!lines.is_empty() && !shown.is_empty());
    if rule == 1 {
        lines.push(Line::from(Span::styled("─".repeat(inner_w), dim)));
    }
    // The rows that fit: when they don't all, those the filter leaves out go first, then a
    // window of them keeps the highlighted one in view.
    let room = fit.saturating_sub(lines.len());
    // An option taller than that room (a narrow screen) never fits: once its item's detail
    // view was read in full, it is listed on a row by its number and first line, there to be
    // picked and answered; else it is not drawn, and not answered.
    let draw_row = |i: usize| -> Vec<Line<'static>> {
        let full = row_lines(i);
        if !(detail_read && matches!(rows[i].kind, ActionKind::Answer(_)) && full.len() > room) {
            return full;
        }
        let label = &rows[i].label;
        let mut first = label.lines().next().unwrap_or_default().to_string();
        if label.trim_end().contains('\n') {
            first.push('…');
        }
        let mark = " (read in full)";
        let text = inner_w.saturating_sub(1);
        let first = truncate_cells(&first, text.saturating_sub(crate::edit::width(mark)));
        row_lines_as(i, &truncate_cells(&format!("{first}{mark}"), text))
    };
    let mut drawn: Vec<usize> = shown.clone();
    let mut windowed = false;
    if shown.iter().map(|&i| draw_row(i).len()).sum::<usize>() > room {
        windowed = true;
        if !popup.filter.is_empty() {
            drawn.retain(|i| matches.contains(i));
        }
        let heights: Vec<usize> = drawn.iter().map(|&i| draw_row(i).len()).collect();
        let at = popup
            .sel
            .and_then(|k| drawn.iter().position(|&i| rows[i].kind == k));
        let mut start = 0;
        if let Some(at) = at {
            while start < at && heights[start..=at].iter().sum::<usize>() > room {
                start += 1;
            }
        }
        let (mut end, mut used) = (start, 0);
        while end < drawn.len() && used + heights[end] <= room {
            used += heights[end];
            end += 1;
        }
        drawn = drawn[start..end].to_vec();
    }
    // Every option must be drawn for the item to be, unless its detail view was read in full:
    // then only an option drawn is answered ([`Panes::popup_partial`]).
    let options_cut = shown
        .iter()
        .any(|i| matches!(rows[*i].kind, ActionKind::Answer(_)) && !drawn.contains(i));
    // Where each row drawn starts and how many lines it takes: a tap on any of them is a tap
    // on the row.
    let mut row_at = Vec::new();
    for &i in &drawn {
        let row = draw_row(i);
        row_at.push((rows[i].kind, lines.len(), row.len()));
        lines.extend(row);
    }
    panes.popup_rows = Some((
        popup.target.clone(),
        drawn.iter().map(|&i| rows[i].kind).collect(),
    ));
    if lines.len() > fit {
        // Still taller than the room (a great many rows): cut, and not drawn in full.
        lines.truncate(fit);
        whole = false;
    }
    exact = exact && lines.iter().all(|l| l.width() <= inner_w.max(1));
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let rect = Rect::new(x, bottom - height, width, height);
    let title = match (&popup.target, item) {
        (ActionTarget::Card(id), _) if id == HUB_THREAD => " twr asks ".to_string(),
        (ActionTarget::Card(_), Some(i)) if i.proposal => confirm_title(state, i),
        (ActionTarget::Card(id), _) => match state.registry.get(id) {
            Some(t) if t.is_orchestrator() => " the orchestrator needs you ".to_string(),
            Some(t) => format!(" {} ", t.name),
            None => " ? ".to_string(),
        },
        (ActionTarget::Request(_), Some(i)) => format!(
            " {} · {} ",
            state
                .registry
                .get(&i.thread)
                .map_or(i.thread.as_str(), |t| t.name.as_str()),
            if i.reason == AttentionReason::Permission {
                "permission"
            } else {
                "question"
            }
        ),
        (ActionTarget::Request(_), None) => " ? ".to_string(),
    };
    let footer = match (popup.filter.is_empty(), windowed) {
        (true, false) => " Enter runs · Esc closes ".to_string(),
        (true, true) => " ↑↓ more rows · Esc closes ".to_string(),
        (false, _) => format!(" filter: {} ", popup.filter),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title(close_title())
        .title_bottom(Line::from(truncate(&footer, inner_w)).right_aligned());
    let inner = block.inner(rect);
    // What it covers is not on screen; its own item is, when drawn whole, and its rows drawn
    // whole are tapped ([`Target::Row`]), its top border closes it.
    panes.cover(Some(rect));
    close_row(panes, rect, Target::Close);
    for (kind, start, n) in row_at {
        if start + n <= lines.len() && n > 0 {
            let y = inner.y + start as u16;
            panes.target(
                Rect::new(inner.x, y, inner.width, n as u16),
                Target::Row(kind),
            );
        }
    }
    let partial = whole && options_cut && detail_read;
    panes.popup_partial = partial;
    if let Some(i) = item
        .filter(|i| whole && (!options_cut || partial) && exact && state.incomplete(i).is_none())
    {
        panes.drawn(DrawnItem {
            thread: card,
            attn_id: i.attn_id.clone(),
            revision: i.revision,
            area: inner,
        });
    }
    panes.add(PaneId::Popup, inner);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
    Some(rect)
}

/// The last report of the worker card `target` is on, for its action popup when nothing waits
/// on it, `width` cells wide: a dim line naming its turn and grade, then the report in full
/// (cards show what of it fits in their slot), or, when only a capped preview of it is known
/// (its fetch failed), the preview, the line saying so. Nothing for anything else.
fn popup_report_rows(
    state: &AppState,
    target: &crate::app::ActionTarget,
    width: usize,
) -> Vec<Line<'static>> {
    let crate::app::ActionTarget::Card(id) = target else {
        return Vec::new();
    };
    let Some(r) = state
        .registry
        .get(id)
        .filter(|t| !t.is_orchestrator())
        .and_then(|t| t.last_report.as_ref())
    else {
        return Vec::new();
    };
    let dim = Style::default().fg(Color::DarkGray);
    // Its summary moved to the blob store and its fetch failed: only a preview is known.
    let capped = r
        .extra
        .get(twr_core::blob::CAPPED_KEY)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|c| c.iter().any(|k| k == "summary"));
    let mut label = "last report".to_string();
    if let Some(turn) = r.turn {
        label.push_str(&format!(" · turn {turn}"));
    }
    label.push_str(match r.grade {
        twr_core::event::Grade::Success => " · success",
        twr_core::event::Grade::Partial => " · partial",
        twr_core::event::Grade::GaveUp => " · gave up",
        twr_core::event::Grade::NeedsVerification => " · needs verification",
    });
    let mut rows = vec![Line::from(Span::styled(truncate_cells(&label, width), dim))];
    // On its own rows, so that a narrow popup never cuts it off.
    if capped {
        rows.extend(
            wrap("a preview: the whole report is in the dive", width.max(1))
                .into_iter()
                .map(|l| Line::from(Span::styled(l, dim))),
        );
    }
    rows.extend(
        wrap(&r.summary, width.max(1))
            .into_iter()
            .map(|l| Line::from(Span::raw(l))),
    );
    rows
}

/// The rows of attention item `item` of card `card` in the action popup, `width` cells wide,
/// without its options (the popup lists them as rows of their own), and whether every row
/// fits the width: a request's badge, summary, context and note ([`request_detail_rows`]); a
/// confirm card's badge, head and actions ([`card_detail_rows`]); any other item's badge and
/// summary, why it asks, its note and how many more wait ([`attention_rows`]). Wrapped once
/// and kept while the item and the width stay the same ([`WrapCache`]).
fn popup_item_rows(
    state: &AppState,
    card: &str,
    item: &AttentionItem,
    width: usize,
) -> (Vec<Line<'static>>, bool) {
    let request = !item.proposal
        && matches!(
            item.reason,
            AttentionReason::Permission | AttentionReason::Question
        )
        && state
            .registry
            .get(&item.thread)
            .is_some_and(|t| !t.is_orchestrator());
    let without_options = |d: &DetailRows, mut rows: Vec<Line<'static>>| {
        rows.extend(d.rows[..d.options.start].iter().cloned());
        // The blank row before the options goes with them.
        while rows.last().is_some_and(|l| l.width() == 0) {
            rows.pop();
        }
        rows.extend(d.rows[d.options.end..].iter().cloned());
        (rows, d.exact)
    };
    if request {
        let d = request_detail_rows(state, item, width);
        return without_options(&d, vec![Line::from(badge(item.reason))]);
    }
    if item.proposal {
        let d = card_detail_rows(state, item, 0, width);
        return without_options(&d, Vec::new());
    }
    let Some(a) = attention_rows(state, card, width, false) else {
        return (Vec::new(), true);
    };
    let options = option_rows(&item.options, width).len();
    let mut rows = a.core[..a.core.len().saturating_sub(options)].to_vec();
    rows.extend(a.extra.iter().cloned());
    (rows, a.exact)
}

/// Key help for what is on screen, most important first (the status bar cuts it from the
/// right). A line break is Shift-Enter where it is known to arrive apart from Enter
/// ([`crate::app::ViewState::shift_enter`]), Ctrl-J elsewhere.
pub fn hints(state: &AppState) -> std::borrow::Cow<'static, str> {
    let hints = screen_hints(state);
    if state.view.shift_enter {
        hints.into()
    } else {
        hints
            .replace("Shift-Enter new line", "Ctrl-J new line")
            .into()
    }
}

/// [`hints`], with Shift-Enter for a line break: one line for each view and what is selected
/// in it, or the popup open over it.
fn screen_hints(state: &AppState) -> &'static str {
    if state.view.screen == Screen::Decisions {
        return "↑↓ select · PgUp/Dn page · Enter open thread · f site · v provider · Esc close · ? help · Ctrl-C quit";
    }
    let compact = state.compact();
    if state.action_popup().is_some() {
        return if compact {
            "1-9 answer · ↑↓/tap pick · Enter/tap again run · type to filter · Esc close"
        } else {
            "1-9 answer · ↑↓/tap pick · Enter/tap again run · type to filter · Tab/Shift-↑↓ other card · Esc close"
        };
    }
    if let Some(s) = state.popup() {
        return if s.mention {
            "↑↓ pick · Tab/Enter insert · Esc close · Ctrl-C quit"
        } else {
            "↑↓ pick · Tab complete · Enter run · Esc close · Ctrl-C quit"
        };
    }
    let dive = state.view.dive.is_some() && state.view.screen == Screen::Stack;
    let main = state.main_focused();
    if compact {
        return compact_hints(state, dive, main);
    }
    match state.view.screen {
        Screen::Grid => {
            "Shift-arrows/Tab select · Enter on an empty line: actions · Enter send · Shift-Enter new line · Esc release · Ctrl-G stack · / commands · ? help · Ctrl-C quit"
        }
        _ if state.view.block.is_some() => {
            "Enter on an empty line: expand/collapse · Shift-↑↓ blocks and requests · Esc release · Ctrl-↑↓ scroll · Shift-→ cards · Enter send · ? help"
        }
        _ if main && dive => {
            "Ctrl-↑↓ scroll · PgUp/Dn page · Shift-→ cards · Esc back to the orchestrator · Enter send · Shift-Enter new line · ? help · Ctrl-C quit"
        }
        _ if main => {
            "Shift-↑↓ blocks and requests · Ctrl-↑↓ scroll · PgUp/Dn page · Shift-→ cards · Esc release · Enter send · ? help · Ctrl-C quit"
        }
        _ if dive && state.esc_does() == EscDoes::Release => {
            "Enter send to this thread · Shift-Enter new line · Esc release · Tab/Shift-↑↓ select · Enter on an empty line: actions · / commands · Ctrl-O tool calls · ? help · Ctrl-C quit"
        }
        _ if dive => {
            "Enter send to this thread · Shift-Enter new line · Esc back to the orchestrator · Tab/Shift-↑↓ select · Enter on an empty line: actions · / commands · Ctrl-O tool calls · ? help · Ctrl-C quit"
        }
        _ => {
            "Enter send · Shift-Enter new line · Tab/Shift-↑↓ select · Enter on an empty line: actions · Shift-← conversation · / commands · @ threads · ↑↓ history · Ctrl-G grid · ? help · Ctrl-C quit"
        }
    }
}

/// [`screen_hints`] on the compact layout: its lists' keys, or its conversation's, which has
/// no cards beside it.
fn compact_hints(state: &AppState, dive: bool, main: bool) -> &'static str {
    if state.compact_listed() {
        return "↑↓/tap select · Enter/tap again: actions · Esc release · /c chat · /n needs you · /t threads · ? help · Ctrl-C quit";
    }
    match () {
        _ if state.view.block.is_some() => {
            "Enter on an empty line: expand/collapse · Shift-↑↓ blocks and requests · Esc release · Ctrl-↑↓ scroll · Enter send · ? help"
        }
        _ if main => {
            "Shift-↑↓ blocks and requests · Ctrl-↑↓ scroll · PgUp/Dn page · Esc release · Enter send · /n needs you · /t threads · ? help"
        }
        _ if dive => {
            "Enter send to this thread · Enter on an empty line: oldest item · Esc back to the orchestrator · /n needs you · /t threads · ? help · Ctrl-C quit"
        }
        _ => {
            "Enter send · Shift-Enter new line · Enter on an empty line: oldest item · /n needs you · /t threads · Shift-↑↓ blocks · ? help · Ctrl-C quit"
        }
    }
}

/// The decisions overlay: its summary, its filters and the table over the whole screen, and
/// the status bar. It has no input box: its keys are its own ([`AppState::decisions_key`]).
fn render_decisions_view(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let [summary, filters, table, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(status_height(state, area)),
    ])
    .areas(area);
    let mut line = decision_summary_line(state);
    if let Some(name) = state.instance_label() {
        line.spans.insert(
            0,
            Span::styled(
                format!(
                    "[{}] ",
                    truncate_cells(name, usize::from(summary.width) / 3)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        );
    }
    frame.render_widget(Paragraph::new(line), summary);
    frame.render_widget(Paragraph::new(decision_filter_line(state)), filters);
    panes.add(PaneId::Text, summary);
    panes.add(PaneId::Text, filters);
    // `✕ close` at the summary's end: its whole row closes the overlay (Esc).
    let close = " ✕ close ";
    let w = crate::edit::width(close) as u16;
    if summary.width > w {
        let at = Rect::new(summary.right() - w, summary.y, w, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                close,
                Style::default().add_modifier(Modifier::BOLD),
            )),
            at,
        );
        panes.target(summary, Target::Close);
    }
    // The filters: a tap on one moves it on, as `f` and `v` do.
    for (needle, target) in [
        (" site ", Target::SiteFilter),
        (" provider ", Target::ProviderFilter),
    ] {
        if let Some(at) = find_text(frame.buffer_mut(), filters, needle) {
            // From its name to the end of its key, `(f)` or `(v)`.
            let key = if target == Target::SiteFilter {
                "(f)"
            } else {
                "(v)"
            };
            let end = find_text(frame.buffer_mut(), filters, key).map_or(at.right(), |k| k.right());
            panes.target(Rect::new(at.x, at.y, end.saturating_sub(at.x), 1), target);
        }
    }
    render_decision_table(frame, table, state, panes);
    render_status(frame, status, state, panes, None);
}

/// `decisions · <kept> [of <seen>] · code N · orchestrator N · user N · route → orchestrator/user a/b (p%)`.
pub fn decision_summary_text(state: &AppState) -> String {
    let log = &state.decisions;
    let s = log.summary();
    let mut out = format!(" decisions · {}", log.len());
    if log.seen() > log.len() as u64 {
        out.push_str(&format!(" kept of {} (last {DECISIONS_CAP})", log.seen()));
    }
    for (p, n) in PROVIDERS.iter().zip(s.by_provider) {
        out.push_str(&format!(" · {} {n}", provider_label(*p)));
    }
    let share = if s.route_settled == 0 {
        "—".to_string()
    } else {
        format!(
            "{:.0}%",
            s.route_fallback as f64 * 100.0 / s.route_settled as f64
        )
    };
    out.push_str(&format!(
        " · route fell through to orchestrator/user: {}/{} ({share})",
        s.route_fallback, s.route_settled
    ));
    out
}

fn decision_summary_line(state: &AppState) -> Line<'static> {
    Line::from(Span::styled(
        decision_summary_text(state),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

fn decision_filter_line(state: &AppState) -> Line<'static> {
    let f = state.view.decision_filter;
    let dim = Style::default().fg(Color::DarkGray);
    let on = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let site = f.site.map_or("all", site_label);
    let provider = f.provider.map_or("all", provider_label);
    Line::from(vec![
        Span::styled(" site ", dim),
        Span::styled(site, if f.site.is_some() { on } else { dim }),
        Span::styled(" (f) · provider ", dim),
        Span::styled(provider, if f.provider.is_some() { on } else { dim }),
        Span::styled(format!(" (v) · {} shown", state.decision_rows().len()), dim),
    ])
}

/// `s` cut to `width` cells of display width ([`crate::edit::width`]), ending in `…` when
/// cut: a wide character with no room before the `…` is left out whole, so the result may be
/// a cell narrower than `width`.
fn truncate_cells(s: &str, width: usize) -> String {
    if crate::edit::width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for (_, g) in clusters(s) {
        let w = cluster_width(g);
        if used + w > width - 1 {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out.push('…');
    out
}

/// `s` cut to `width` characters, ending in `…` when cut.
pub fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// Fixed decision table columns (header, width); options take the rest.
const DECISION_COLUMNS: [(&str, u16); 8] = [
    ("time", 8),
    ("thread", 12),
    ("turn", 4),
    ("site", 10),
    ("provider", 12),
    ("chosen", 16),
    ("conf", 4),
    ("latency", 8),
];

/// The cells of one decision row: time, thread, turn, site, provider, chosen, confidence,
/// latency, options (cut to `options_width`).
pub fn decision_cells(state: &AppState, r: &DecisionRow, options_width: usize) -> [String; 9] {
    let thread = if r.thread == HUB_THREAD {
        HUB_THREAD.to_string()
    } else {
        state
            .registry
            .get(&r.thread)
            .map_or_else(|| r.thread.clone(), |t| t.name.clone())
    };
    [
        r.time.clone(),
        truncate(&thread, 12),
        r.turn.map_or_else(|| "—".to_string(), |t| t.to_string()),
        site_label(r.site).to_string(),
        provider_label(r.provider).to_string(),
        truncate(&r.chosen, 16),
        format!("{:.2}", r.confidence),
        format!("{}ms", r.latency_ms),
        truncate(&r.options.join(", "), options_width),
    ]
}

/// First visible table row so that `selected` shows when `fit` rows fit.
pub fn table_first_row(selected: usize, fit: usize) -> usize {
    (selected + 1).saturating_sub(fit.max(1))
}

fn render_decision_table(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let dim = Style::default().fg(Color::DarkGray);
    let rows = state.decision_rows();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dim)
        .title(" decision.made · newest first ");
    let inner = block.inner(area);
    panes.add(PaneId::Decisions, inner);
    if rows.is_empty() {
        let msg = if state.decisions.is_empty() {
            "no decisions yet"
        } else {
            "no decisions match the filters (f site, v provider)"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(msg, dim))
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
        return;
    }
    let fixed: u16 =
        DECISION_COLUMNS.iter().map(|(_, w)| w).sum::<u16>() + DECISION_COLUMNS.len() as u16;
    let options_width = usize::from(inner.width.saturating_sub(fixed).max(1));
    let fit = usize::from(inner.height.saturating_sub(1)).max(1);
    let selected = state.decision_selected();
    let first = table_first_row(selected, fit);
    // Its rows under the header: a tap selects one, a tap on the selected one opens it.
    for (k, r) in rows.iter().skip(first).take(fit).enumerate() {
        let y = inner.y + 1 + k as u16;
        if y < inner.bottom() {
            panes.target(
                Rect::new(inner.x, y, inner.width, 1),
                Target::Decision(r.id.clone()),
            );
        }
    }
    let body: Vec<Row> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(fit)
        .map(|(i, r)| {
            let row = Row::new(
                decision_cells(state, r, options_width)
                    .into_iter()
                    .map(Cell::from),
            );
            if i == selected {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect();
    let mut widths: Vec<Constraint> = DECISION_COLUMNS
        .iter()
        .map(|(_, w)| Constraint::Length(*w))
        .collect();
    widths.push(Constraint::Fill(1));
    let header = Row::new(
        DECISION_COLUMNS
            .iter()
            .map(|(h, _)| *h)
            .chain(["options"])
            .map(|h| Cell::from(Span::styled(h, dim.add_modifier(Modifier::BOLD)))),
    );
    let mut block = block;
    if first > 0 {
        block = block.title_bottom(Line::from(format!("↑ {first} newer")).left_aligned());
    }
    let below = rows.len() - (first + fit).min(rows.len());
    if below > 0 {
        block = block.title_bottom(Line::from(format!("{below} older ↓")).right_aligned());
    }
    frame.render_widget(
        Table::new(body, widths)
            .header(header)
            .column_spacing(1)
            .block(block),
        area,
    );
}

/// The status bar: a pending confirmation or the last status, then `parked` (the
/// note on hidden parked threads when the card area could not draw it) and the hints, with
/// the subscriptions' remaining usage at the start of the last row ([`usage_spans`]).
fn render_status(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    panes: &mut Panes,
    parked: Option<String>,
) {
    panes.add(PaneId::Text, area);
    let width = usize::from(area.width);
    let mut rows = status_head(state, width, parked.clone());
    // The head's rows as wrapped, for where its notice and notes landed, and how far the
    // usage segment pushes the last one's in.
    let head_rows: Vec<String> = rows.iter().map(line_text).collect();
    let mut last_shift = 0;
    // A question waiting for `y` is all the bar says.
    if state.view.confirm.is_none() {
        if let Some(last) = rows.last_mut() {
            let dim = Style::default().fg(Color::DarkGray);
            let head: usize = last
                .spans
                .iter()
                .map(|s| crate::edit::width(&s.content))
                .sum();
            let avail = width.saturating_sub(head);
            let hints = hints(state);
            let hints_w = crate::edit::width(&hints);
            match usage_spans(state, avail, hints_w, chrono::Utc::now(), &chrono::Local) {
                Some(usage) => {
                    let used: usize = usage.iter().map(|s| crate::edit::width(&s.content)).sum();
                    let keep = avail.saturating_sub(used + 1);
                    let text = truncate_cells(&hints, keep);
                    last_shift = used + 1;
                    let head = std::mem::replace(&mut last.spans, usage);
                    last.spans.push(Span::raw(" "));
                    last.spans.extend(head);
                    last.spans.push(Span::styled(text, dim));
                }
                None => last.spans.push(Span::styled(hints, dim)),
            }
        }
    }
    // Rows past the bar's height: the last one it has says the rest is cut.
    let fit = usize::from(area.height).max(1);
    if rows.len() > fit {
        rows.truncate(fit);
        if let Some(last) = rows.last_mut() {
            last.spans
                .push(Span::styled("…", Style::default().fg(Color::DarkGray)));
        }
    }
    frame.render_widget(Paragraph::new(rows), area);
    // What a tap does here: the question's answers; else the notice's rows dismiss it, and a
    // note does what its key does.
    let buf = frame.buffer_mut();
    if state.view.confirm.is_some() {
        for (needle, target) in [(CONFIRM_YES, Target::Yes), (CONFIRM_NO, Target::No)] {
            if let Some(cells) = find_text(buf, area, needle) {
                panes.target(cells, target);
            }
        }
        return;
    }
    // The head's text before it was wrapped: the notice first, the notes last.
    let text: String = status_head_spans(state, parked.clone())
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    let shown = &head_rows[..head_rows.len().min(fit)];
    let last = head_rows.len().saturating_sub(1);
    let shift = |r: usize| if r == last { last_shift } else { 0 };
    let mut place = |range: Range<usize>, target: Target| {
        for cells in wrapped_cells(&text, shown, range, area, shift) {
            panes.target(cells, target.clone());
        }
    };
    if let Some(msg) = &state.view.status {
        place(0..msg.len() + 1, Target::Notice);
    }
    let parts = status_note_parts(state, parked);
    if !parts.is_empty() {
        // Each note and the separator after it.
        let sep = " · ".len();
        let joined: usize = parts.iter().map(|(n, _)| n.len() + sep).sum();
        let mut at = text.len().saturating_sub(joined);
        for (note, target) in parts {
            if let Some(t) = target {
                place(at..at + note.len(), t);
            }
            at += note.len() + sep;
        }
    }
}

/// A line's text, its spans one after the other.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The cells where bytes `range` of `text` were drawn once `text` was wrapped into `rows`
/// (the rows' text, drawn from the top of `area`, row `r` pushed `shift(r)` cells in): a rect
/// per row it is on. Wrapping drops the spaces it breaks at; a row that is not the text's
/// next one ends the search.
fn wrapped_cells(
    text: &str,
    rows: &[String],
    range: Range<usize>,
    area: Rect,
    shift: impl Fn(usize) -> usize,
) -> Vec<Rect> {
    let mut out = Vec::new();
    let mut p = 0;
    for (r, row) in rows.iter().enumerate() {
        while !text[p..].starts_with(row.as_str()) && text[p..].starts_with([' ', '\n']) {
            p += 1;
        }
        if !text[p..].starts_with(row.as_str()) {
            break;
        }
        let (a, b) = (range.start.max(p), range.end.min(p + row.len()));
        if a < b && text.is_char_boundary(a) && text.is_char_boundary(b) {
            let x = shift(r) + crate::edit::width(&text[p..a]);
            let w = crate::edit::width(&text[a..b]);
            let cells = Rect::new(
                area.x.saturating_add(u16::try_from(x).unwrap_or(u16::MAX)),
                area.y.saturating_add(u16::try_from(r).unwrap_or(u16::MAX)),
                u16::try_from(w).unwrap_or(u16::MAX),
                1,
            );
            out.push(cells.intersection(area));
        }
        p += row.len();
    }
    out
}

/// Fewest cells the hints keep beside the usage segment (all of them when shorter).
const HINTS_MIN: usize = 30;

/// The usage segment at the start of the status bar's last row, for `avail` cells beside
/// its head with `hints` cells of hints, at `now` (reset times in `tz`): the widest form of
/// [`twr_core::usage::segment`] that fits beside the complete hints; when none does, the
/// widest that fits in two fifths of the row while the hints keep at least [`HINTS_MIN`]
/// cells; none when even the compact one does not, or nothing is known. A window with 25%
/// left or less is yellow, one with 10% or less red and bold; data ten minutes old is grey,
/// or dimmed (not bold) when it is near a limit.
fn usage_spans<Tz: chrono::TimeZone>(
    state: &AppState,
    avail: usize,
    hints: usize,
    now: chrono::DateTime<chrono::Utc>,
    tz: &Tz,
) -> Option<Vec<Span<'static>>>
where
    Tz::Offset: std::fmt::Display,
{
    use twr_core::usage::{segment, Form, Level, Part};
    let shown = state.usage.shown(now);
    if shown.is_empty() {
        return None;
    }
    // The widest form in `room` cells that leaves the hints `keep` cells (and a gap).
    let widest = |room: usize, keep: usize| {
        Form::ALL.iter().find_map(|&form| {
            let parts = segment(&shown, form, now, tz);
            let w: usize = parts.iter().map(|(t, _)| crate::edit::width(t)).sum();
            (w <= room && w + 1 + keep <= avail).then_some(parts)
        })
    };
    let beside = avail.saturating_sub(hints + 1);
    let parts = widest(beside, hints).or_else(|| widest(avail * 2 / 5, hints.min(HINTS_MIN)))?;
    let dim = Style::default().fg(Color::DarkGray);
    Some(
        parts
            .into_iter()
            .map(|(text, part)| {
                let style = match part {
                    Part::Plain | Part::Age => dim,
                    Part::Reset => Style::default().fg(Color::Red),
                    Part::Window(Level::Normal, true) => dim,
                    Part::Window(Level::Normal, false) => Style::default().fg(Color::Gray),
                    Part::Window(Level::Warn, stale) => {
                        let s = Style::default().fg(Color::Yellow);
                        if stale {
                            s.add_modifier(Modifier::DIM)
                        } else {
                            s
                        }
                    }
                    Part::Window(Level::Low, stale) => {
                        let s = Style::default().fg(Color::Red);
                        if stale {
                            s.add_modifier(Modifier::DIM)
                        } else {
                            s.add_modifier(Modifier::BOLD)
                        }
                    }
                };
                Span::styled(text, style)
            })
            .collect(),
    )
}

/// The status bar's notes: background tasks, the sidebar is hidden (and how many hidden
/// threads need you), Focus mode, then `parked` (the note on hidden parked threads, when the
/// cards have no room for it); each with what a tap on it does ([`Target`]).
fn status_note_parts(state: &AppState, parked: Option<String>) -> Vec<(String, Option<Target>)> {
    let mut notes = Vec::new();
    let background: usize = state
        .registry
        .list()
        .iter()
        .map(|t| t.background_tasks.len())
        .sum();
    if background > 0 {
        notes.push((format!("{background} background task(s)"), None));
    }
    if state.view.sidebar_hidden && state.view.screen == Screen::Stack {
        let needs = state
            .order()
            .iter()
            .filter(|id| {
                state.top_attention(id).is_some()
                    || state
                        .registry
                        .get(id)
                        .is_some_and(|t| t.status() == ThreadStatus::Error)
            })
            .count();
        let waiting = if needs > 0 {
            format!(", {needs} need you")
        } else {
            String::new()
        };
        notes.push((
            format!("sidebar hidden{waiting} · Ctrl-B shows"),
            Some(Target::Sidebar),
        ));
    }
    if state.view.focus_mode {
        notes.push((
            format!("Focus · {} hidden · Ctrl-F all", state.hidden_in_focus()),
            Some(Target::FocusOff),
        ));
    }
    notes.extend(parked.map(|p| (p, Some(Target::Parked))));
    notes
}

/// [`status_note_parts`] as the status bar says them, joined.
fn status_notes(state: &AppState, parked: Option<String>) -> Option<String> {
    let notes: Vec<String> = status_note_parts(state, parked)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    (!notes.is_empty()).then(|| notes.join(" · "))
}

/// The question waiting for `y`'s `yes` in the status bar, to tap ([`Target::Yes`]): its
/// spaces do not break, so it is never cut across rows.
pub const CONFIRM_YES: &str = "[\u{a0}yes\u{a0}]";
/// Its `no` ([`Target::No`]).
pub const CONFIRM_NO: &str = "[\u{a0}no\u{a0}]";

/// The status bar but its hints, wrapped at `width`: the question waiting for `y` (all it
/// says then), else the last notice and the notes ([`status_notes`]), each in full. The hints
/// follow on the last row ([`render_status`]).
fn status_head(state: &AppState, width: usize, parked: Option<String>) -> Vec<Line<'static>> {
    wrap_line(Line::from(status_head_spans(state, parked)), width.max(1))
}

/// [`status_head`] before it is wrapped.
fn status_head_spans(state: &AppState, parked: Option<String>) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    if let Some(c) = &state.view.confirm {
        spans.push(Span::styled(
            format!(" {} ", c.prompt),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        // Its answers to tap ([`Target::Yes`]), each kept whole on a row.
        let button = Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(CONFIRM_YES, button));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(CONFIRM_NO, button));
        return spans;
    }
    if let Some(msg) = &state.view.status {
        spans.push(Span::styled(
            format!("{msg} "),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("| ", dim));
    }
    spans.extend(load_spans(state));
    if !state.disk_low.is_empty() {
        let boxes: Vec<String> = state
            .disk_low
            .iter()
            .map(|(b, free)| format!("{b} {} free", twr_core::git::human_bytes(*free)))
            .collect();
        spans.push(Span::styled(
            format!("disk low: {}", boxes.join(", ")),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
    }
    if let Some(note) = status_notes(state, parked) {
        spans.push(Span::styled(format!("{note} · "), dim));
    }
    spans
}

/// The machine's load, while it is busy: what presses, `busy: cpu 52% · ` in yellow, or
/// `saturated: cpu 97% mem 31% · ` in bold red. Nothing while it is calm or unknown.
fn load_spans(state: &AppState) -> Vec<Span<'static>> {
    use twr_core::load::Level;
    let Some(load) = &state.load else {
        return Vec::new();
    };
    let pressing = load.pressing();
    if pressing.is_empty() {
        return Vec::new();
    }
    let level = load.level();
    let style = match level {
        Level::Saturated => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(Color::Yellow),
    };
    let what: Vec<String> = pressing.into_iter().map(|(text, _)| text).collect();
    vec![
        Span::styled(format!("{}: {}", level.as_str(), what.join(" ")), style),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
    ]
}

/// Height of the status bar on a screen `area`: the rows its question or notice and notes
/// wrap into ([`status_head`], counting the note on hidden parked threads wherever it is
/// drawn), at most a third of the screen, at least one.
fn status_height(state: &AppState, area: Rect) -> u16 {
    let rows = status_head(state, usize::from(area.width), parked_note(state)).len();
    let cap = usize::from(area.height / 3).max(1);
    u16::try_from(rows.clamp(1, cap)).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An approval's text keeps every space when wrapped: quoted repeated spaces across a
    /// row's end read as they run, where word wrapping dropped the ones at the break. Both a
    /// permission's summary and a request's whole input (`input:`) wrap this way.
    #[test]
    fn approval_text_wraps_without_losing_spaces() {
        let text = "echo 'a      b' && grep 'x  y'";
        for width in 3..text.len() + 2 {
            let rows = wrap_exact(Line::from(Span::raw(text.to_string())), width);
            let joined: String = rows.iter().map(|(l, _)| line_text(l)).collect();
            assert_eq!(joined, text, "width {width}");
            assert!(
                rows.iter().all(|(l, _)| l.width() <= width),
                "width {width}"
            );
            assert!(rows.iter().skip(1).all(|(_, j)| *j == Join::Glue));
        }
        // Word wrapping, which other text keeps, drops spaces at a break.
        let words = wrap_joined(Line::from(Span::raw(text.to_string())), 10);
        let joined: String = words.iter().map(|(l, _)| line_text(l)).collect();
        assert_ne!(joined, text);
        // The input field of a request's context.
        let rows = context_rows(&[("input", "a     b")], 6);
        let joined: String = rows.iter().map(|(l, _)| line_text(l)).collect();
        assert_eq!(joined, "input: a     b");
    }
    use crate::app::tests::{five_threads, key, thread_item, three_threads, with_decisions};
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;
    use twr_core::event::{AgentText, AgentToolCall, AgentToolResult, DecisionProvider, EventBody};
    use twr_core::testkit::ThreadScript;

    /// `lines` as one line of words: what wrapped text says, wherever it was cut.
    fn flat(lines: &[String]) -> String {
        lines
            .iter()
            .flat_map(|l| l.split_whitespace())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn draw(state: &AppState, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            render(f, state);
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    fn screen(lines: &[String]) -> String {
        lines.join("\n")
    }

    /// Draws `state` at `w` x `h` keeping the panes (as the IO loop does); returns the rows.
    fn draw_panes(state: &mut AppState, w: u16, h: u16) -> Vec<String> {
        let buf = crate::app::tests::draw(state, w, h);
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// The text of pane `id` as last drawn, its border included, one row per line.
    fn pane_text(state: &AppState, lines: &[String], id: &PaneId) -> String {
        let a = state
            .view
            .panes
            .get(id)
            .unwrap_or_else(|| panic!("{id:?}"))
            .area;
        let (x0, w) = (usize::from(a.x - 1), usize::from(a.width + 2));
        lines[usize::from(a.y - 1)..usize::from(a.bottom() + 1)]
            .iter()
            .map(|l| l.chars().skip(x0).take(w).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn card(id: &str) -> PaneId {
        PaneId::Thread(id.into())
    }

    /// Column of the first occurrence of `needle` in `row`, in cells.
    fn col(row: &str, needle: &str) -> Option<usize> {
        row.find(needle).map(|b| row[..b].chars().count())
    }

    /// The orchestrator conversation, 40 replies long, beside [`crate::app::tests::with_orch`]'s
    /// cards, with `t-alpha`'s card selected.
    fn conversation_beside_cards() -> AppState {
        let mut st = crate::app::tests::without_worker_requests(crate::app::tests::with_orch());
        st.transcript.as_mut().unwrap().settle();
        let mut o = ThreadScript::new("t-orch");
        o.at(100);
        for i in 0..40 {
            st.apply_event(&crate::app::tests::thread_item(
                &o.text(&format!("reply-{i}")),
            ));
        }
        st.set_focus(Some("t-alpha".into()));
        st
    }

    /// The pane selected is drawn so: Shift-← moves the thick yellow border from the selected
    /// card to the conversation (whose thick border, in the color of where text goes, turns
    /// yellow), whose bottom border then says it is selected and how to get back, next to how
    /// much is below once Ctrl-↑ scrolled it; Shift-→ puts it back on the card. So at every
    /// width: beside a column of cards, under a row of them, and with no room for the whole
    /// label.
    #[test]
    fn the_selected_pane_has_the_thick_border() {
        use crossterm::event::KeyModifiers;
        let thick = |text: &str| text.contains('┃') || text.contains('━');
        let shift = |code| KeyEvent::new(code, KeyModifiers::SHIFT);
        for (w, label) in [
            (160, "selected · Shift-↑↓ blocks · Shift-→ cards"),
            (80, "selected · Shift-→ cards"),
            (44, "selected"),
            (30, "selected"),
        ] {
            let mut st = conversation_beside_cards();
            let lines = draw_panes(&mut st, w, 30);
            if w >= 80 {
                assert!(thick(&pane_text(&st, &lines, &card("t-alpha"))), "{w}");
            }
            assert!(!screen(&lines).contains("selected ·"), "{w}");
            assert!(
                !screen(&lines).contains(" selected "),
                "{w}: the conversation is where text goes, not selected"
            );
            st.handle_key(shift(KeyCode::Left));
            let lines = draw_panes(&mut st, w, 30);
            let s = screen(&lines);
            if w >= 80 {
                assert!(
                    !thick(&pane_text(&st, &lines, &card("t-alpha"))),
                    "{w}: {s}"
                );
            }
            let main = pane_text(&st, &lines, &PaneId::Transcript);
            assert!(thick(&main), "{w}: {s}");
            assert!(main.lines().last().unwrap().contains(label), "{w}: {s}");
            st.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
            draw_panes(&mut st, w, 30);
            let lines = draw_panes(&mut st, w, 30);
            let bottom = pane_text(&st, &lines, &PaneId::Transcript)
                .lines()
                .last()
                .unwrap()
                .to_string();
            assert!(
                bottom.contains(label) && bottom.contains("↓ 1"),
                "{w}: {bottom}"
            );
            st.handle_key(shift(KeyCode::Right));
            let lines = draw_panes(&mut st, w, 30);
            assert!(!screen(&lines).contains("selected ·"), "{w}");
            if w >= 80 {
                assert!(thick(&pane_text(&st, &lines, &card("t-alpha"))), "{w}");
            }
        }
    }

    /// Too narrow for a column of cards, a dive gives the conversation the whole height: no
    /// row of cards, a header band naming the thread and saying Esc gets back. Esc brings
    /// the row back, with the card selected before the dive selected again. So on phone
    /// sizes and at 80x24; from 100 columns the cards stay beside the dive.
    #[test]
    fn a_narrow_dive_hides_the_cards_and_esc_brings_them_back() {
        let cards = |st: &AppState| {
            st.view
                .panes
                .panes
                .iter()
                .filter(|p| matches!(p.id, PaneId::Thread(_)))
                .count()
        };
        for (w, h) in [(30, 20), (40, 20), (45, 30), (80, 24), (99, 24)] {
            let mut st = conversation_beside_cards();
            st.set_focus(Some("t-beta".into()));
            draw_panes(&mut st, w, h);
            assert!(cards(&st) > 0, "{w}x{h}");
            st.dive("t-gamma");
            let lines = draw_panes(&mut st, w, h);
            let s = screen(&lines);
            assert_eq!(cards(&st), 0, "{w}x{h}: {s}");
            assert!(
                lines[0].contains("gamma") && lines[0].contains("Esc"),
                "{w}x{h}: {s}"
            );
            let main = st.view.panes.get(&PaneId::Transcript).unwrap().area;
            assert_eq!(
                main.y, 1,
                "{w}x{h}: the conversation starts under the header"
            );
            st.handle_key(key(KeyCode::Esc));
            assert_eq!(st.view.dive, None);
            let lines = draw_panes(&mut st, w, h);
            assert!(cards(&st) > 0, "{w}x{h}: {}", screen(&lines));
            assert_eq!(st.focused(), Some("t-beta"), "{w}x{h}");
        }
        // Wide enough for the column, the cards stay beside the dive.
        let mut st = conversation_beside_cards();
        st.dive("t-gamma");
        draw_panes(&mut st, 100, 30);
        assert!(cards(&st) > 0);
    }

    /// On a phone-sized screen, a dive's header keeps saying how to get back, `‹ back` to tap
    /// and the key: a long name gives up `talking to`, then its end.
    #[test]
    fn a_narrow_dive_header_keeps_esc() {
        let mut st = conversation_beside_cards();
        let mut t = ThreadScript::new("t-long");
        t.at(60);
        crate::app::tests::control(&mut st, &[t.created("invoice-rounding-for-eu")]);
        st.dive("t-long");
        for (w, head) in [
            (30, " ‹ back  invoice-…  Esc: back"),
            (40, " ‹ back  invoice-rounding-f…  Esc: back"),
            (50, " ‹ back  invoice-rounding-for-eu  Esc: back"),
            (60, " ‹ back  talking to invoice-rounding-for-eu  Esc: back"),
        ] {
            let lines = draw_panes(&mut st, w, 20);
            assert!(lines[0].contains(head), "{w}: {}", lines[0]);
        }
    }

    /// A taller input box lists the same cards: the box is under the main pane, and the
    /// column of cards keeps its height beside it. (It once spanned the whole width, so a
    /// growing box took its rows from the column too and the last card there went, e.g.
    /// `t-eps` at 160x32.)
    #[test]
    fn a_taller_input_box_lists_the_same_cards() {
        let drawn = |st: &AppState| -> Vec<String> {
            st.view
                .panes
                .panes
                .iter()
                .filter_map(|p| match &p.id {
                    PaneId::Thread(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        };
        for (w, h) in [(160, 56), (160, 32), (160, 24), (120, 20), (100, 33)] {
            let mut st = five_threads();
            draw_panes(&mut st, w, h);
            let nav = drawn(&st);
            st.view.input.set("one\ntwo\nthree");
            let lines = draw_panes(&mut st, w, h);
            assert_eq!(drawn(&st), nav, "{w}x{h}: {}", screen(&lines));
            let input = st.view.panes.get(&PaneId::Input).unwrap().area;
            let column = st.view.panes.get(&card(&nav[0])).unwrap().area;
            assert!(input.right() < column.x, "{w}x{h}: {input:?} {column:?}");
        }
        assert_eq!(
            drawn(&{
                let mut st = five_threads();
                st.view.input.set("one\ntwo\nthree");
                draw_panes(&mut st, 160, 56);
                st
            }),
            ["t-beta", "t-alpha", "t-delta", "t-gamma", "t-eps"]
        );
    }

    /// A narrow dive hides the cards, so no key selects one: Tab and the Shift-arrows say so
    /// and the dive's card stays selected, so the action popup Enter opens is the thread's the
    /// header names. A dive that began wider, with another card selected, selects its own card
    /// once the screen is narrow; Esc still brings back the card selected before the dive.
    #[test]
    fn a_narrow_dive_keeps_its_own_card_selected() {
        use crossterm::event::KeyModifiers;
        let mut st = conversation_beside_cards();
        st.set_focus(Some("t-gamma".into()));
        draw_panes(&mut st, 80, 24);
        // Enter, Enter on the card dives in.
        st.handle_key(key(KeyCode::Enter));
        draw_panes(&mut st, 80, 24);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
        draw_panes(&mut st, 80, 24);
        for k in [
            key(KeyCode::Tab),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
        ] {
            st.handle_key(k);
            assert_eq!(st.focused(), Some("t-gamma"), "{k:?}");
            assert_eq!(
                st.view.status.as_deref(),
                Some(crate::app::DIVE_HIDES_CARDS)
            );
        }
        st.handle_key(key(KeyCode::Enter));
        draw_panes(&mut st, 80, 24);
        for c in "stop".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [crate::app::Action::Stop {
                thread: "t-gamma".into()
            }]
        );
        // Begun wide, with another card selected during it, then narrowed.
        let mut st = conversation_beside_cards();
        st.set_focus(Some("t-beta".into()));
        st.dive("t-gamma");
        draw_panes(&mut st, 160, 40);
        st.handle_key(key(KeyCode::Esc));
        st.dive("t-gamma");
        draw_panes(&mut st, 160, 40);
        st.handle_key(key(KeyCode::Tab));
        assert_ne!(st.focused(), Some("t-gamma"));
        draw_panes(&mut st, 80, 24);
        assert_eq!(st.focused(), Some("t-gamma"));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.focused(), Some("t-beta"));
    }

    /// A hidden thread dived into leaves the listing with the dive: Esc still selects the card
    /// selected before it.
    #[test]
    fn leaving_a_dive_into_a_hidden_thread_selects_the_card_before_it() {
        let mut st = conversation_beside_cards();
        let mut z = ThreadScript::new("t-zeta");
        z.at(60);
        crate::app::tests::control(&mut st, &[z.created("zeta"), z.parked()]);
        assert!(!st.order().contains(&"t-zeta".to_string()));
        let second = st.order()[1].clone();
        st.set_focus(Some(second.clone()));
        st.dive("t-zeta");
        draw_panes(&mut st, 160, 40);
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None);
        assert_eq!(st.focused(), Some(second.as_str()));
    }

    /// The sidebar hidden during a dive and shown after it: it selects the card selected
    /// before the dive.
    #[test]
    fn a_sidebar_hidden_during_a_dive_shows_the_card_from_before_it() {
        let mut st = conversation_beside_cards();
        let second = st.order()[1].clone();
        st.set_focus(Some(second.clone()));
        st.dive("t-gamma");
        draw_panes(&mut st, 160, 40);
        st.handle_key(crate::app::tests::ctrl('b'));
        assert!(st.view.sidebar_hidden);
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None);
        st.handle_key(crate::app::tests::ctrl('b'));
        assert!(!st.view.sidebar_hidden);
        assert_eq!(st.focused(), Some(second.as_str()));
    }

    /// From one dive to another with the sidebar hidden, then out and the sidebar shown: it
    /// selects the card selected before the first dive, as it does with the sidebar shown.
    #[test]
    fn a_chained_dive_with_the_sidebar_hidden_shows_the_card_from_before_it() {
        let mut st = conversation_beside_cards();
        let second = st.order()[1].clone();
        st.set_focus(Some(second.clone()));
        st.dive("t-gamma");
        draw_panes(&mut st, 160, 40);
        st.handle_key(crate::app::tests::ctrl('b'));
        st.dive("t-beta");
        st.handle_key(key(KeyCode::Esc));
        st.handle_key(crate::app::tests::ctrl('b'));
        assert!(!st.view.sidebar_hidden);
        assert_eq!(st.focused(), Some(second.as_str()));
    }

    /// The dive's header, the input box's title and the hints say what the next Esc does:
    /// back to the orchestrator with nothing selected, `Esc: release` with a card selected,
    /// `Esc: close` with the action popup open; at every width the header keeps it.
    #[test]
    fn the_dive_header_says_what_esc_does() {
        let mut st = conversation_beside_cards();
        st.dive("t-gamma");
        let header = |st: &AppState, w: u16| draw(st, w, 30)[0].clone();
        let s = screen(&draw(&st, 160, 30));
        assert!(s.contains("Esc returns to the orchestrator"), "{s}");
        assert!(
            s.contains("Enter sends · Esc: back to the orchestrator"),
            "{s}"
        );
        st.handle_key(key(KeyCode::Tab));
        assert!(st.focused().is_some());
        let s = screen(&draw(&st, 160, 30));
        assert!(header(&st, 160).contains("· Esc: release"), "{s}");
        assert!(
            !s.contains("Esc returns") && !s.contains("Esc: back"),
            "{s}"
        );
        assert!(s.contains("Enter sends · Esc: release"), "{s}");
        assert!(hints(&st).contains("Esc release"), "{}", hints(&st));
        let (name, esc) = main_title(&st, 20);
        assert_eq!(esc, "Esc: release", "{name}");
        st.handle_key(key(KeyCode::Esc));
        assert!(header(&st, 160).contains("Esc returns to the orchestrator"));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.action.is_some(), "the dived thread's popup");
        let s = screen(&draw(&st, 160, 30));
        assert!(header(&st, 160).contains("· Esc: close"), "{s}");
        assert!(s.contains("Enter sends · Esc: close"), "{s}");
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(main_title(&st, 13).1, "Esc: back");
    }

    /// A dive's header keeps `Esc: back` with only a cell or two left for the name.
    #[test]
    fn a_dive_header_keeps_esc_with_little_room() {
        let mut st = conversation_beside_cards();
        st.dive("t-gamma");
        assert_eq!(
            main_title(&st, 13),
            ("g…".to_string(), "Esc: back".to_string())
        );
        assert_eq!(main_title(&st, 12).1, "Esc: back");
    }

    /// On the narrowest screen the help's indent leaves room for what an entry does: no row is
    /// wider than the overlay.
    #[test]
    fn the_help_fits_a_tiny_screen() {
        for width in [1, 2, 3, 4] {
            let lines = help_lines(width);
            assert!(
                lines.iter().all(|l| l.width() <= width),
                "{width}: {lines:?}"
            );
            let text: String = lines
                .iter()
                .map(|l| l.to_string())
                .collect::<String>()
                .split_whitespace()
                .collect();
            assert!(
                text.contains("/open[thread]talktoaworkerdirectly"),
                "{width}: {text}"
            );
        }
    }

    /// The card selected during a dive stays selected after it (`/back`; Esc would release
    /// it first): only a dive with nothing selected gives the selection from before it back.
    #[test]
    fn a_card_selected_during_a_dive_stays_selected_after_it() {
        let mut st = conversation_beside_cards();
        st.set_focus(Some("t-beta".into()));
        st.dive("t-gamma");
        draw_panes(&mut st, 160, 40);
        st.set_focus(Some("t-alpha".into()));
        for c in "/back".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.dive, None);
        assert_eq!(st.focused(), Some("t-alpha"));
    }

    /// The status bar's hints say how to select the conversation, and once it is, how to
    /// scroll it and get back to the cards.
    #[test]
    fn the_hints_follow_the_selected_pane() {
        let mut st = conversation_beside_cards();
        assert!(hints(&st).contains("Shift-← conversation"));
        st.handle_key(KeyEvent::new(
            KeyCode::Left,
            crossterm::event::KeyModifiers::SHIFT,
        ));
        let h = hints(&st);
        assert!(
            h.starts_with("Shift-↑↓ blocks and requests") && h.contains("Shift-→ cards"),
            "{h}"
        );
        st.view.block = Some("block:x".into());
        let h = hints(&st);
        assert!(
            h.starts_with("Enter on an empty line: expand/collapse")
                && h.contains("Ctrl-↑↓ scroll")
                && h.contains("Shift-→ cards"),
            "{h}"
        );
        let help = help_lines(1000)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(help.contains("select the conversation, the cards again"));
    }

    #[test]
    fn the_connected_instance_is_visible_in_every_view() {
        let mut st = crate::app::tests::with_orch();
        st.instance_name = Some("work".into());
        for view in [Screen::Stack, Screen::Grid, Screen::Decisions] {
            st.view.screen = view;
            let lines = draw(&st, 120, 40);
            assert!(screen(&lines).contains("[work]"), "{}", screen(&lines));
        }
        let mut st = AppState::new();
        st.instance_name = Some("personal".into());
        st.view.screen = Screen::Grid;
        assert!(screen(&draw(&st, 80, 24)).contains("worker threads · [personal]"));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render(f, &st);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer
            .content
            .iter()
            .any(|c| c.symbol() == "┌" && c.fg == Color::DarkGray));
    }

    fn with_tabs(st: &mut AppState, current: &str, tabs: &[(&str, crate::workspace::TabHub)]) {
        st.workspace = Some(current.into());
        st.instance_name = Some(current.into());
        st.set_tabs(
            tabs.iter()
                .map(|(name, hub)| crate::workspace::Tab {
                    name: name.to_string(),
                    hub: *hub,
                })
                .collect(),
        );
    }

    /// With more than one workspace a strip of tabs tops every view, the current one
    /// highlighted, each with its badges; it names the workspace in place of the label.
    #[test]
    fn the_tab_strip_tops_every_view_with_each_workspaces_badges() {
        use crate::workspace::TabHub;
        let mut st = crate::app::tests::with_orch();
        with_tabs(
            &mut st,
            "a",
            &[
                (
                    "default",
                    TabHub::Running {
                        threads: 3,
                        running: 1,
                        permissions: 1,
                        questions: 1,
                    },
                ),
                ("a", TabHub::Unknown),
                ("b", TabHub::Stopped),
                ("c", TabHub::Busy),
            ],
        );
        for (w, h) in [(160, 42), (80, 24)] {
            for view in [Screen::Stack, Screen::Grid, Screen::Decisions] {
                st.view.screen = view;
                let lines = draw(&st, w, h);
                let strip = lines[0].trim_end();
                assert!(
                    strip.starts_with(" default ⚠2 ●1 │ a"),
                    "{view:?} {w}: {strip}"
                );
                assert!(strip.ends_with("│ b ○ │ c …"), "{view:?} {w}: {strip}");
                assert!(!screen(&lines).contains("[a]"), "{}", screen(&lines));
            }
        }
        // The current tab is highlighted.
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| {
            render(f, &st);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let at = lines_col(&draw(&st, 80, 24)[0], " a");
        assert!(buf[(at as u16 + 1, 0)]
            .modifier
            .contains(Modifier::REVERSED));
        assert!(!buf[(1, 0)].modifier.contains(Modifier::REVERSED));
        // One workspace: no strip, the label as before.
        with_tabs(&mut st, "default", &[("default", TabHub::Unknown)]);
        st.view.screen = Screen::Stack;
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("[default]"), "{s}");
    }

    /// A delete card's detail view shows the commands it quotes in the view's workspace; a
    /// confirm card quoting a runtime's command, and a runtime request, show theirs as raised.
    #[test]
    fn card_details_localize_only_delete_cards() {
        use twr_core::event::EventBody;
        use twr_core::testkit::ThreadScript;
        let mut st = AppState::new();
        st.attach_via = " -w a".into();
        let mut h = ThreadScript::new(HUB_THREAD);
        let mut card = h.raise("d1", AttentionReason::Question);
        if let EventBody::AttentionRaised(p) = &mut card.body {
            p.summary = "Delete a? `twr trash restore` brings it back".into();
            p.ext = serde_json::json!({"twr": {"delete": {
                "threads": [{"thread": "t-a", "name": "a"}], "asked_by": "user"}}})
            .as_object()
            .cloned();
        }
        let mut proposal = h.raise("c1", AttentionReason::Question);
        if let EventBody::AttentionRaised(p) = &mut proposal.body {
            p.summary = "Allow a's request? Bash: echo `twr thread list`".into();
        }
        let mut t = ThreadScript::new("t-a");
        let mut perm = t.raise("p1", AttentionReason::Permission);
        if let EventBody::AttentionRaised(p) = &mut perm.body {
            p.summary = "Bash: echo `twr thread list`".into();
        }
        crate::app::tests::control(&mut st, &[t.created("a"), perm, card, proposal]);
        let text = |rows: &DetailRows| {
            rows.rows
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let hub = st.attention.get("d1").unwrap().clone();
        let detail = text(&card_detail_rows(&st, &hub, 0, 80));
        assert!(detail.contains("`twr -w a trash restore`"), "{detail}");
        let confirm = st.attention.get("c1").unwrap().clone();
        let detail = text(&card_detail_rows(&st, &confirm, 0, 80));
        assert!(detail.contains("echo `twr thread list`"), "{detail}");
        assert!(!detail.contains("-w a"), "{detail}");
        let request = st.attention.get("p1").unwrap().clone();
        let detail = text(&request_detail_rows(&st, &request, 80));
        assert!(detail.contains("echo `twr thread list`"), "{detail}");
        assert!(!detail.contains("-w a"), "{detail}");
    }

    /// A permission's approval view (inline card and popup) shows the runtime's marked text
    /// exactly, across wraps: the marks for zero-width and format characters
    /// (`⟨U+1BCA0⟩`, `⟨U+13430⟩`), runs of spaces, and the whole input in `input:`.
    #[test]
    fn the_approval_view_shows_marked_text_exactly() {
        use twr_core::event::EventBody;
        use twr_core::testkit::ThreadScript;
        let mut st = AppState::new();
        let summary = "Allow Bash: rm⟨U+1BCA0⟩ -rf⟨U+13430⟩ ~ 'a     b'… 9 more characters; \
                       open the card to see all";
        let input = "rm⟨U+1BCA0⟩ -rf⟨U+13430⟩ ~ 'a     b'    && ls⟨U+0020⟩";
        let mut t = ThreadScript::new("t-a");
        let mut perm = t.raise("p1", AttentionReason::Permission);
        if let EventBody::AttentionRaised(p) = &mut perm.body {
            p.summary = summary.into();
            p.ext = serde_json::json!({"claude": {"full_input": input}})
                .as_object()
                .cloned();
        }
        crate::app::tests::control(&mut st, &[t.created("a"), perm]);
        let request = st.attention.get("p1").unwrap().clone();
        for width in [11, 17, 23, 40] {
            let rows = request_detail_rows(&st, &request, width);
            let joined: String = rows.rows.iter().map(|l| l.to_string()).collect();
            assert!(joined.contains(summary), "width {width}: {joined}");
            assert!(
                joined.contains(&format!("input: {input}")),
                "width {width}: {joined}"
            );
        }
    }

    /// On a short terminal an overlay stays below the strip: the tabs stay on screen, and
    /// a click on the overlay's top row never lands on a tab hidden under it.
    #[test]
    fn overlays_stay_below_the_tab_strip() {
        use crate::workspace::TabHub;
        let mut st = crate::app::tests::with_orch();
        with_tabs(
            &mut st,
            "default",
            &[("default", TabHub::Unknown), ("a", TabHub::Stopped)],
        );
        st.view.help = true;
        let lines = draw(&st, 80, 10);
        assert!(lines[0].starts_with(" default"), "{}", screen(&lines));
        assert!(lines[0].contains("│ a ○"), "{}", screen(&lines));
        st.view.help = false;
        st.view.picker = Some(crate::app::Picker {
            runtimes: vec![("claude-code".into(), Some(true))],
            selected: 0,
            default: "claude-code".into(),
            model: None,
            switch: false,
        });
        let lines = draw(&st, 80, 6);
        assert!(lines[0].starts_with(" default"), "{}", screen(&lines));
        assert!(lines[0].contains("│ a ○"), "{}", screen(&lines));
    }

    fn lines_col(row: &str, needle: &str) -> usize {
        col(row, needle).unwrap_or_else(|| panic!("no {needle:?} in {row}"))
    }

    /// Tabs that don't fit: the ones around the current one, with markers for the rest.
    #[test]
    fn tabs_that_do_not_fit_show_around_the_current_one() {
        use crate::workspace::TabHub;
        let names: Vec<String> = (1..=12).map(|i| format!("workspace-{i:02}")).collect();
        let tabs: Vec<(&str, TabHub)> = names
            .iter()
            .map(|n| (n.as_str(), TabHub::Stopped))
            .collect();
        let mut st = crate::app::tests::with_orch();
        with_tabs(&mut st, "workspace-06", &tabs);
        let strip = draw(&st, 60, 24)[0].clone();
        assert!(strip.starts_with("‹ "), "{strip}");
        assert!(strip.trim_end().ends_with(" ›"), "{strip}");
        assert!(strip.contains("workspace-06"), "{strip}");
        assert!(!strip.contains("workspace-01"), "{strip}");
        // At the start, only the right one.
        with_tabs(&mut st, "workspace-01", &tabs);
        let strip = draw(&st, 60, 24)[0].clone();
        assert!(strip.starts_with(" workspace-01"), "{strip}");
        assert!(strip.trim_end().ends_with(" ›"), "{strip}");
    }

    /// The TUI starts typing: the input box shows, ready to send to the orchestrator.
    #[test]
    fn empty_state() {
        let st = AppState::new();
        let s = screen(&draw(&st, 80, 24));
        assert!(s.contains(EMPTY_STATE), "{s}");
        assert!(!s.contains("INPUT"), "no mode badge: {s}");
        assert!(s.contains("Enter sends"), "{s}");
    }

    #[test]
    fn hiding_sidebar_reclaims_space_and_prevents_answering_hidden_cards() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let toggle = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        // Option keys answer while navigating; typed, they would only edit the input line.
        for width in [80, 160] {
            let mut st = AppState::new();
            let mut w = ThreadScript::new("t-beta");
            let mut e = ThreadScript::new("t-error");
            let created = w.created("beta");
            let started = w.turn_started("build");
            let mut permission =
                w.raise("permission", twr_core::event::AttentionReason::Permission);
            if let EventBody::AttentionRaised(p) = &mut permission.body {
                p.summary = "Allow build?".into();
            }
            crate::app::tests::control(
                &mut st,
                &[
                    created,
                    started,
                    permission,
                    e.created("error"),
                    e.turn_started("test"),
                    e.turn_ended(TurnResult::Error),
                ],
            );
            st.set_focus(Some("t-beta".into()));
            draw_panes(&mut st, width, 40);
            st.note_drawn(std::time::Instant::now());
            assert!(st.view.shown.is_some());
            let before = st.view.panes.get(&PaneId::Transcript).unwrap().area;
            st.handle_key(toggle);
            assert!(st.view.shown.is_none());
            st.view.status = None;
            st.handle_key(key(KeyCode::Enter));
            assert_eq!(st.view.action, None, "no card selected: no popup");
            let lines = draw_panes(&mut st, width, 40);
            let after = st.view.panes.get(&PaneId::Transcript).unwrap().area;
            assert!(st.view.panes.get(&card("t-beta")).is_none());
            // The request is drawn in the conversation only, where nothing answers it until
            // it is selected there (Shift-↓, a click), also once it has been on screen a while.
            assert!(st.view.panes.drawn_on("t-beta").is_some());
            st.note_drawn(std::time::Instant::now());
            assert!(st.view.shown.is_none(), "nothing selected to answer");
            st.handle_key(key(KeyCode::Enter));
            assert_eq!(st.view.action, None);
            assert!(st.view.input.is_empty());
            assert_eq!(after.width, width - 2, "the full width inside the border");
            if width < SIDE_MIN_WIDTH {
                assert!(after.height > before.height, "the card row is gone");
            } else {
                assert!(after.width > before.width, "the side column is gone");
            }
            let s = flat(&lines);
            assert!(
                s.contains("2 need you") && s.contains("Ctrl-B shows"),
                "{s}"
            );
            st.handle_key(toggle);
            draw_panes(&mut st, width, 40);
            assert!(st.view.panes.get(&card("t-beta")).is_some());
        }
    }

    #[test]
    fn hidden_sidebar_keeps_orchestrator_attention_and_the_open_thread() {
        use crate::app::tests::{control, orch_events};
        use crossterm::event::{KeyEvent, KeyModifiers};
        let mut st = three_threads();
        let mut o = ThreadScript::new("t-orch");
        let mut events = orch_events(&mut o, "t-orch");
        events.push(o.raise(
            "orch-permission",
            twr_core::event::AttentionReason::Permission,
        ));
        control(&mut st, &events);
        st.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(st.nav_order(), ["t-orch"]);
        draw_panes(&mut st, 160, 40);
        assert!(st.view.panes.drawn_on("t-orch").is_some());
        assert!(st.view.panes.get(&card("t-beta")).is_none());
        st.dive("t-gamma");
        let lines = draw_panes(&mut st, 160, 40);
        assert!(st.view.sidebar_hidden);
        assert_eq!(st.main_thread().as_deref(), Some("t-gamma"));
        assert!(lines[0].contains("talking to gamma"));
        assert!(st.nav_order().is_empty());
        assert!(st.view.panes.drawn_on("t-orch").is_none());
        st.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(st.main_thread().as_deref(), Some("t-gamma"));
        assert!(st.order().contains(&"t-gamma".to_string()));
    }

    #[test]
    fn focus_mode_draws_only_relevant_threads_and_explains_an_empty_view() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let mut st = five_threads();
        st.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        st.view.status = None;
        for width in [80, 160] {
            let lines = draw_panes(&mut st, width, 50);
            assert!(st.view.panes.get(&card("t-eps")).is_none());
            assert!(st.view.panes.get(&card("t-beta")).is_some());
            assert!(screen(&lines).contains("Focus · 1 hidden · Ctrl-F all"));
        }
        st.handle_key(key(KeyCode::Char('g')));
        draw_panes(&mut st, 160, 50);
        assert!(st.view.panes.get(&card("t-eps")).is_none());
        assert!(st.view.panes.get(&card("t-beta")).is_some());

        let mut empty = AppState::new();
        let mut w = ThreadScript::new("done");
        for ev in [
            w.created("done"),
            w.turn_started("work"),
            w.turn_ended(TurnResult::Completed),
        ] {
            empty.apply_control(&ev);
        }
        empty.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        for view in [Screen::Stack, Screen::Grid] {
            empty.view.screen = view;
            let s = screen(&draw(&empty, 160, 24));
            assert!(s.contains("no threads working or needing you"), "{s}");
            assert!(!s.contains(EMPTY_STATE), "{s}");
        }
    }

    /// Wide: the main pane on the left, the worker cards in a column on the right, the
    /// permission first; each card says status, runtime, model and mode.
    #[test]
    fn three_threads_permission_is_first_with_card_text() {
        let mut st = three_threads();
        st.set_focus(Some("t-alpha".into()));
        let lines = draw_panes(&mut st, 120, 40);
        let at = |id: &str| st.view.panes.get(&card(id)).unwrap().area;
        let (beta, alpha, gamma) = (at("t-beta"), at("t-alpha"), at("t-gamma"));
        assert!(beta.y < alpha.y && alpha.y < gamma.y, "{}", screen(&lines));
        assert_eq!(beta.y, 1, "the permission card is the first card");
        assert_eq!(
            beta.x,
            120 - side_width(120) + 1,
            "the cards are on the right"
        );
        // Fixed heights: a card with an item, one without (however little it says).
        assert_eq!(
            gamma.height + 2,
            COMPACT_CARD_HEIGHT,
            "no attention: compact"
        );
        assert_eq!(at("t-beta").height + 2, CARD_HEIGHT);

        let beta_card = pane_text(&st, &lines, &card("t-beta"));
        assert!(beta_card.contains("waiting · fake"), "{beta_card}");
        assert!(
            beta_card.contains("manual ┐"),
            "the mode, top right: {beta_card}"
        );
        // The project is in the title, not on a row of its own.
        assert!(beta_card.contains("┌ proj · beta "), "{beta_card}");
        assert_eq!(beta_card.matches("proj").count(), 1, "{beta_card}");
        assert!(beta_card.contains(" PERMISSION  Bash: rm -rf target"));
        assert!(beta_card.contains("1 allow  2 deny"));
        assert!(beta_card.contains("note: build dir only"));

        let alpha_card = pane_text(&st, &lines, &card("t-alpha"));
        assert!(alpha_card.contains("running · fake"), "{alpha_card}");
        assert!(alpha_card.contains(" REVIEW  turn gave up"));
        assert!(alpha_card.contains("1 retry  2 dismiss"));

        let gamma_card = pane_text(&st, &lines, &card("t-gamma"));
        assert!(gamma_card.contains("running · fake"));
        assert!(!gamma_card.contains("PERMISSION"));

        // alpha is selected: thick border. The main pane and the input line are the
        // orchestrator's whatever is selected.
        assert!(alpha_card.starts_with('┏'), "{alpha_card}");
        let s = screen(&lines);
        assert!(
            lines[0].starts_with(" ORCHESTRATOR  starts with your first message "),
            "{s}"
        );
        // The empty input box is there, saying who a line goes to.
        assert!(st.view.panes.get(&PaneId::Input).is_some(), "{s}");
        assert!(s.contains("message the orchestrator…"), "{s}");
        assert!(
            s.contains("This is your conversation with the orchestrator."),
            "{s}"
        );
    }

    /// The text of the main pane's header (its band, the top row).
    fn header(lines: &[String], w: usize) -> String {
        lines[0].chars().take(w).collect()
    }

    /// A working card shows a frame of the icon before its status, a waiting one none; the
    /// frame follows the clock, never moves the layout, and keeps the clock ticking.
    #[test]
    fn working_cards_show_the_icon_and_waiting_ones_do_not() {
        let mut st = three_threads();
        st.view.animate = true;
        st.view.frame = 6;
        let lines = draw_panes(&mut st, 120, 40);
        let gamma = pane_text(&st, &lines, &card("t-gamma"));
        assert!(gamma.contains("│▆ running · fake"), "{gamma}");
        assert!(!gamma.contains("twr"), "the cursor alone: {gamma}");
        let beta = pane_text(&st, &lines, &card("t-beta"));
        assert!(beta.contains("│waiting · fake"), "{beta}");
        assert!(!beta.contains('▆'), "a waiting card does not move: {beta}");
        assert!(
            st.view.panes.animated,
            "a moving icon keeps the clock ticking"
        );
        let areas: Vec<Rect> = st.view.panes.panes.iter().map(|p| p.area).collect();

        st.view.frame = 3;
        let lines = draw_panes(&mut st, 120, 40);
        let gamma = pane_text(&st, &lines, &card("t-gamma"));
        assert!(gamma.contains("│▃ running · fake"), "{gamma}");
        let after: Vec<Rect> = st.view.panes.panes.iter().map(|p| p.area).collect();
        assert_eq!(areas, after, "frames never move the layout");
        // The icon's cells (and the space after it) are not the card's text.
        let pane = st.view.panes.get(&card("t-gamma")).unwrap();
        assert_eq!(
            pane.skip,
            vec![Rect::new(pane.area.x, pane.area.y, icon::WIDTH + 1, 1)]
        );
    }

    /// The icon never costs a card a row: a status line that fits without it gives way at
    /// its end.
    #[test]
    fn the_icon_never_adds_a_row() {
        let mut st = three_threads();
        st.view.animate = true;
        let lines = draw_panes(&mut st, 120, 40);
        let text = pane_text(&st, &lines, &card("t-gamma"));
        // The activity line right under the status: the status took one row.
        assert!(
            text.lines().nth(2).is_some_and(|l| l.starts_with("│› ")),
            "status and activity, as without the icon:\n{text}"
        );
        assert!(text.contains("│_ running · fake · default model"), "{text}");
        // Three 33-column cards in a row: 31 cells fit the status line, not the icon too.
        let lines = draw_panes(&mut st, 99, 40);
        let text = pane_text(&st, &lines, &card("t-gamma"));
        let rows: Vec<&str> = text.lines().collect();
        assert!(
            rows[1].contains("│_ running · fake · default mod…│"),
            "{text}"
        );
        assert!(rows[0].contains("┌ proj · gamma "), "{text}");
        assert!(rows[2].contains("│› working"), "{text}");
        assert!(rows[3].trim_matches(['│', ' ']).is_empty(), "{text}");
    }

    /// Nothing working, nothing moves: no icon and no clock.
    #[test]
    fn nothing_working_draws_no_icon_and_needs_no_tick() {
        use crate::app::tests::control;
        let mut st = three_threads();
        st.view.animate = true;
        let mut a = ThreadScript::new("t-alpha");
        let mut c = ThreadScript::new("t-gamma");
        a.at(50);
        c.at(60);
        control(
            &mut st,
            &[
                a.turn_ended(TurnResult::Completed),
                c.turn_ended(TurnResult::Completed),
            ],
        );
        let lines = draw_panes(&mut st, 120, 40);
        assert!(!screen(&lines).contains("twr"), "{}", screen(&lines));
        assert!(!st.view.panes.animated);
        let epoch = std::time::Instant::now();
        assert_eq!(icon::next_tick(st.view.panes.animated, epoch, epoch), None);
    }

    /// `[tui] animate = false`: the still `_` marks what works, and nothing ticks.
    #[test]
    fn without_animation_the_marker_stands_still() {
        let mut st = three_threads();
        st.view.animate = false;
        st.view.frame = 6;
        let lines = draw_panes(&mut st, 120, 40);
        let gamma = pane_text(&st, &lines, &card("t-gamma"));
        assert!(gamma.contains("│_ running"), "{gamma}");
        assert!(!screen(&lines).contains('▆'));
        assert!(!st.view.panes.animated, "a still marker needs no clock");
        st.view.screen = Screen::Grid;
        let lines = draw_panes(&mut st, 120, 40);
        assert!(screen(&lines).contains("│_ running"), "{}", screen(&lines));
        assert!(!st.view.panes.animated);
    }

    /// A running thread's item that does not block it (a review) is drawn in full next to
    /// the icon: its option keys answer it, and it counts as seen.
    #[test]
    fn an_item_next_to_the_icon_is_still_drawn_in_full() {
        let mut st = three_threads();
        st.view.animate = true;
        st.view.frame = 5;
        st.set_focus(Some("t-alpha".into()));
        let mut buf = Buffer::empty(Rect::default());
        for (w, h) in [(120, 40), (80, 40)] {
            buf = crate::app::tests::draw(&mut st, w, h);
            st.note_drawn(std::time::Instant::now());
            let alpha = st.view.panes.drawn_on("t-alpha").expect("drawn in full");
            assert_eq!((alpha.attn_id.as_str(), alpha.revision > 0), ("rv1", true));
            assert_eq!(
                st.view.shown.as_ref().map(|s| s.attn_id.as_str()),
                Some("rv1"),
                "{w}x{h}"
            );
        }
        let rows: Vec<String> = (0..40)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let alpha = pane_text(&st, &rows, &card("t-alpha"));
        assert!(alpha.contains("┃▅ running"), "{alpha}");
        assert!(alpha.contains("1 retry  2 dismiss"), "{alpha}");
        // The next frame keeps the card as it was first drawn: it has been on screen since.
        let since = st.view.shown.as_ref().unwrap().since;
        st.view.frame = 6;
        crate::app::tests::draw(&mut st, 80, 40);
        st.note_drawn(std::time::Instant::now());
        assert_eq!(st.view.shown.as_ref().unwrap().since, since);
        // Grid tiles too.
        st.view.screen = Screen::Grid;
        let lines = draw_panes(&mut st, 120, 40);
        assert!(screen(&lines).contains("┃▆ running"), "{}", screen(&lines));
        assert!(st.view.panes.drawn_on("t-alpha").is_some());
        assert!(st.view.panes.animated);
    }

    /// The orchestrator's header leads with the icon while it works, not while it waits on
    /// the user or rests; a dive's header while the thread dived into works.
    #[test]
    fn the_headers_animate_while_their_thread_works() {
        use crate::app::tests::{control, with_orch};
        let mut st = with_orch();
        st.view.animate = true;
        st.view.frame = 4;
        let lines = draw_panes(&mut st, 120, 40);
        let head = header(&lines, 80);
        assert!(head.starts_with(" ORCHESTRATOR  claude-code"), "{head}");
        assert!(!head.contains('▄'), "idle: {head}");

        let mut o = ThreadScript::new("t-orch");
        o.at(200);
        control(&mut st, &[o.turn_started("what needs me?")]);
        let lines = draw_panes(&mut st, 120, 40);
        let head = header(&lines, 80);
        assert!(
            head.starts_with(" ▄ ORCHESTRATOR  claude-code"),
            "working: {head}"
        );
        assert!(st.view.panes.animated);

        control(&mut st, &[o.raise("op1", AttentionReason::Question)]);
        let lines = draw_panes(&mut st, 120, 40);
        let head = header(&lines, 80);
        assert!(!head.contains('▄'), "waiting on the user: {head}");

        st.dive("t-gamma");
        let lines = draw_panes(&mut st, 120, 40);
        let head = header(&lines, 80);
        assert!(
            head.starts_with(" ▄ ‹ back  talking to gamma  fake · "),
            "dive: {head}"
        );
        st.dive("t-beta");
        let lines = draw_panes(&mut st, 120, 40);
        let head = header(&lines, 80);
        assert!(
            head.starts_with(" ‹ back  talking to beta  fake · "),
            "{head}"
        );
    }

    /// An empty transcript whose history is still coming in says it loads, not that the
    /// thread has nothing to show; once its replay is applied, or there is none to wait for,
    /// the usual hint is back.
    #[test]
    fn a_loading_transcript_says_so() {
        let mut st = crate::app::tests::with_orch();
        let s = screen(&draw(&st, 120, 40));
        assert!(
            s.contains("Loading the conversation with the orchestrator…"),
            "{s}"
        );
        assert!(!s.contains("This is your conversation"), "{s}");
        st.transcript.as_mut().unwrap().resume(0, 0);
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("This is your conversation"), "{s}");

        let mut st = crate::app::tests::three_threads();
        st.dive("t-gamma");
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("Loading gamma's transcript…"), "{s}");
        assert!(!s.contains("Nothing from gamma yet"), "{s}");
        st.transcript.as_mut().unwrap().resume(0, 0);
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("Nothing from gamma yet"), "{s}");
        // A subscription that failed leaves nothing to wait for.
        st.dive("t-beta");
        st.transcript_stalled();
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("Nothing from beta yet"), "{s}");
        // Review: a transcript kept while still empty came back as not loading, and said
        // "Nothing from beta yet" while its new subscription was being made. Asking for one
        // makes it wait again.
        st.dive("t-gamma");
        st.dive("t-beta");
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("Nothing from beta yet"), "{s}");
        assert_eq!(st.transcript_subscribing("t-beta"), 0);
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("Loading beta's transcript…"), "{s}");
        // Only the main pane's transcript, when it is that thread's, is touched.
        assert_eq!(st.transcript_subscribing("t-gamma"), 0);
        assert!(st.transcripts.iter().all(|t| t.catch_up.is_none()));
    }

    /// Review: only the draw right after a replayed event was skipped, so a key, a tick or a
    /// control event drew the replay half applied. Until it is all applied the pane shows the
    /// transcript as the replay began: the loading hint on a first visit, what was kept on a
    /// return.
    #[test]
    fn a_replay_half_applied_is_never_drawn() {
        use twr_core::event::TurnResult;
        let mut st = crate::app::tests::three_threads();
        st.dive("t-gamma");
        let mut g = ThreadScript::new("t-gamma");
        let evs = [
            g.turn_started("first"),
            g.text("kept text"),
            g.turn_ended(TurnResult::Completed),
            g.turn_started("second"),
            g.text("replayed text"),
            g.turn_ended(TurnResult::Completed),
        ];
        let apply = |st: &mut AppState, evs: &[twr_core::event::Event]| {
            for ev in evs {
                st.apply_event(&crate::app::tests::thread_item(ev));
            }
        };
        assert_eq!(st.transcript_subscribing("t-gamma"), 0);
        st.transcript.as_mut().unwrap().resume(0, evs[2].seq);
        apply(&mut st, &evs[..2]);
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("Loading gamma's transcript…"), "{s}");
        assert!(!s.contains("kept text"), "{s}");
        apply(&mut st, &evs[2..3]);
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("kept text"), "{s}");
        assert!(st.transcript.as_ref().unwrap().shown.is_none());

        // Away and back: the kept transcript shows while what came since is applied.
        st.dive("t-beta");
        st.dive("t-gamma");
        assert_eq!(st.transcript_subscribing("t-gamma"), evs[2].seq);
        st.transcript
            .as_mut()
            .unwrap()
            .resume(evs[2].seq, evs[5].seq);
        apply(&mut st, &evs[3..5]);
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("kept text"), "{s}");
        assert!(!s.contains("replayed text"), "{s}");
        apply(&mut st, &evs[5..]);
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("replayed text"), "{s}");
        // Review: a replay of the last events only (it starts past seq 0) showed the "earlier
        // events not shown" notice alone until it was applied, not the loading hint.
        st.dive("t-alpha");
        let mut a = ThreadScript::new("t-alpha");
        let tail = [
            a.turn_started("old"),
            a.text("tail one"),
            a.text("tail two"),
        ];
        st.transcript_subscribing("t-alpha");
        st.transcript
            .as_mut()
            .unwrap()
            .resume(tail[0].seq, tail[2].seq);
        apply(&mut st, &tail[1..2]);
        let s = screen(&draw(&st, 120, 40));
        assert!(s.contains("Loading alpha's transcript…"), "{s}");
        assert!(!s.contains("earlier events not shown"), "{s}");
        apply(&mut st, &tail[2..]);
        let s = screen(&draw(&st, 120, 40));
        let notice = format!("earlier events not shown (from seq {})", tail[1].seq);
        assert!(s.contains(&notice) && s.contains("tail two"), "{s}");
        st.dive("t-gamma");
        // A replay that stops half way shows what it applied.
        st.dive("t-beta");
        st.dive("t-gamma");
        st.transcript
            .as_mut()
            .unwrap()
            .resume(evs[5].seq, evs[5].seq + 2);
        st.apply_event(&crate::app::tests::thread_item(&g.text("partial text")));
        assert!(!screen(&draw(&st, 120, 40)).contains("partial text"));
        st.transcript_stalled();
        assert!(screen(&draw(&st, 120, 40)).contains("partial text"));
    }

    /// [`crate::app::tests::with_orch`] with the orchestrator in a turn, its conversation
    /// `entries`, the clock at frame 5 (`▅`).
    fn orch_working(entries: Vec<Entry>) -> AppState {
        use crate::app::tests::{control, with_orch};
        let mut st = crate::app::tests::without_worker_requests(with_orch());
        let mut o = ThreadScript::new("t-orch");
        o.at(200);
        control(&mut st, &[o.turn_started("what needs me?")]);
        st.transcript.as_mut().unwrap().set_entries(entries);
        st.view.animate = true;
        st.view.frame = 5;
        st
    }

    fn prompt(text: &str) -> Entry {
        Entry::TurnStart {
            seq: 0,
            turn: Some(1),
            from: TurnFrom::User,
            prompt: text.into(),
            notice: false,
            withdrawn: false,
        }
    }

    /// The rows of the transcript pane as drawn, without trailing spaces.
    fn transcript_rows(st: &AppState, lines: &[String]) -> Vec<String> {
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        lines[usize::from(a.y)..usize::from(a.bottom())]
            .iter()
            .map(|l| {
                l.chars()
                    .skip(usize::from(a.x))
                    .take(usize::from(a.width))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Draws `st` at 120 x 30; returns the transcript pane's rows.
    fn drawn_rows(st: &mut AppState) -> Vec<String> {
        let lines = draw_panes(st, 120, 30);
        transcript_rows(st, &lines)
    }

    /// While the orchestrator works, the accent cursor alone waits below the last line of the
    /// conversation: on the clock's frame, out of copied text, keeping the clock ticking.
    #[test]
    fn the_cursor_waits_below_the_conversation_while_the_orchestrator_works() {
        let mut st = orch_working(vec![prompt("what needs me?")]);
        let buf = crate::app::tests::draw(&mut st, 120, 30);
        let lines: Vec<String> = (0..30)
            .map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let rows = transcript_rows(&st, &lines);
        assert_eq!(rows[..3], ["❯ what needs me?", "", "▅"]);
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap();
        let at = Rect::new(pane.area.x, pane.area.y + 2, 1, 1);
        // The prompt's gutter (`❯ `), then the cursor.
        assert_eq!(
            pane.skip,
            vec![Rect::new(pane.area.x, pane.area.y, 2, 1), at]
        );
        assert_eq!(buf[(at.x, at.y)].fg, icon::ACCENT);
        assert!(st.view.panes.animated);
        // The cursor alone, no `twr` letters.
        assert_eq!(rows.iter().filter(|r| r.contains("twr")).count(), 0);

        // Without animation: a still `_`, and no clock.
        st.view.animate = false;
        let lines = draw_panes(&mut st, 120, 30);
        assert_eq!(transcript_rows(&st, &lines)[2], "_");
        assert!(!st.view.panes.animated);
    }

    /// A selection takes the cells it leaves out (the working cursor) from the
    /// draw its text is copied from, not from the draw the drag started on: selecting "hello"
    /// while a reply streams in, then releasing once "hello world" is drawn, copies "hello
    /// world", never "helloworld_".
    #[test]
    fn a_selection_leaves_out_the_cursor_of_the_draw_it_copies() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let text = |t: &str| Entry::Text {
            seq: 0,
            text: t.into(),
            open: true,
        };
        let mut st = orch_working(vec![prompt("hi"), text("hello")]);
        st.view.animate = false;
        let now = std::time::Instant::now();
        let screen = draw_offscreen(&mut st, 120, 30, now);
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let left = MouseButton::Left;
        let (from, to) = ((a.x, a.y + 2), (a.right() - 1, a.y + 2));
        assert!(st
            .handle_mouse(mouse(MouseEventKind::Down(left), from.0, from.1), &screen)
            .is_empty());
        let screen = draw_offscreen(&mut st, 120, 30, now);
        assert!(st
            .handle_mouse(mouse(MouseEventKind::Drag(left), to.0, to.1), &screen)
            .is_empty());
        // The reply streams on: the cursor moves past "world".
        st.transcript.as_mut().unwrap().entries = vec![prompt("hi"), text("hello world")];
        let screen = draw_offscreen(&mut st, 120, 30, now);
        let row: String = (a.x..a.right())
            .map(|x| screen[(x, a.y + 2)].symbol())
            .collect();
        assert_eq!(
            row.trim_end(),
            "● hello world_",
            "the reply and its cursor are drawn"
        );
        let actions = st.handle_mouse(mouse(MouseEventKind::Up(left), to.0, to.1), &screen);
        let [crate::app::Action::Copy { text }] = &actions[..] else {
            panic!("{actions:?}");
        };
        assert_eq!(text, "hello world");
        // The bounds stay where the drag put them.
        let sel = st.view.selection.as_ref().unwrap();
        assert_eq!((sel.anchor, sel.head), (from.into(), to.into()));
    }

    /// A moving cell is noted only where it shows: on a terminal too narrow or
    /// too short for the transcript and the header's icon (1x24, no rows, no columns, 1x1), a
    /// working orchestrator notes no moving cell and schedules no tick; at 2x2 the transcript
    /// has no cell and notes nothing, and only the header's icon, on screen, moves.
    #[test]
    fn a_terminal_too_small_for_the_icon_schedules_no_tick() {
        let text = |t: &str, open: bool| Entry::Text {
            seq: 0,
            text: t.into(),
            open,
        };
        let epoch = std::time::Instant::now();
        for entries in [
            vec![prompt("what needs me?")],
            vec![prompt("hi"), text("Checking the threads", true)],
        ] {
            for (w, h) in [(1, 24), (80, 0), (0, 24), (1, 1)] {
                let mut st = orch_working(entries.clone());
                let buf = draw_offscreen(&mut st, w, h, epoch);
                assert!(
                    st.view.panes.moving.is_empty(),
                    "{w}x{h}: {:?}",
                    st.view.panes.moving
                );
                assert!(!st.view.panes.animated, "{w}x{h}");
                assert_eq!(
                    icon::next_tick(st.view.panes.animated, epoch, epoch),
                    None,
                    "{w}x{h}"
                );
                for p in &st.view.panes.panes {
                    for c in &p.skip {
                        assert_eq!(buf.area.intersection(*c), *c, "{w}x{h}: {c:?} off screen");
                    }
                }
            }
        }
        // 2x2: the header's icon shows (" ▅" over the pane's bottom border); the transcript,
        // with no cell inside its borders, places no cursor.
        for entries in [
            vec![prompt("what needs me?")],
            vec![prompt("hi"), text("Checking the threads", true)],
        ] {
            let mut st = orch_working(entries);
            let buf = draw_offscreen(&mut st, 2, 2, epoch);
            assert_eq!(st.view.panes.moving, vec![Rect::new(1, 0, 1, 1)]);
            assert_eq!(buf[(1, 0)].symbol(), "▅");
            assert!(st
                .view
                .panes
                .get(&PaneId::Transcript)
                .is_none_or(|p| p.skip.is_empty()));
        }
        // Where the icon shows, every moving cell is on screen and shows the cursor's frame.
        for (w, h) in [(2, 3), (3, 24), (10, 6), (80, 24)] {
            let mut st = orch_working(vec![prompt("hi"), text("Checking", true)]);
            let buf = draw_offscreen(&mut st, w, h, epoch);
            assert_eq!(
                st.view.panes.animated,
                !st.view.panes.moving.is_empty(),
                "{w}x{h}"
            );
            for c in &st.view.panes.moving {
                assert_eq!(buf.area.intersection(*c), *c, "{w}x{h}: {c:?} off screen");
                assert_eq!(buf[(c.x, c.y)].symbol(), "▅", "{w}x{h}: {c:?}");
            }
        }
        let mut st = orch_working(vec![prompt("hi")]);
        draw_offscreen(&mut st, 80, 24, epoch);
        assert!(st.view.panes.animated);
    }

    /// An overlay that hides every moving cell stops the clock; the clock
    /// follows what is on screen once the overlays are drawn, not what was drawn under them.
    #[test]
    fn an_overlay_hiding_every_icon_stops_the_clock() {
        let mut st = orch_working(vec![prompt("what needs me?")]);
        draw_panes(&mut st, 80, 24);
        assert!(st.view.panes.animated, "the header and the cursor move");
        assert!(!st.view.panes.moving.is_empty());
        st.view.help = true;
        let lines = draw_panes(&mut st, 80, 24);
        assert!(screen(&lines).contains("twr help"));
        assert!(
            st.view.panes.moving.is_empty(),
            "{:?}",
            st.view.panes.moving
        );
        assert!(
            !st.view.panes.animated,
            "nothing moving shows under the help"
        );
        let epoch = std::time::Instant::now();
        assert_eq!(icon::next_tick(st.view.panes.animated, epoch, epoch), None);
        st.view.help = false;
        draw_panes(&mut st, 80, 24);
        assert!(st.view.panes.animated);
        // Every moving cell noted shows the cursor's frame.
        let buf = crate::app::tests::draw(&mut st, 80, 24);
        for c in &st.view.panes.moving {
            assert_eq!(buf[(c.x, c.y)].symbol(), "▅", "{c:?}");
        }
    }

    /// A reply streaming in has the cursor at its end; one that fills its last row, on the
    /// next row.
    #[test]
    fn the_cursor_follows_a_reply_streaming_in() {
        let text = |t: &str, open: bool| Entry::Text {
            seq: 0,
            text: t.into(),
            open,
        };
        let mut st = orch_working(vec![prompt("hi"), text("Checking the threads", true)]);
        let lines = draw_panes(&mut st, 120, 30);
        let rows = transcript_rows(&st, &lines);
        assert_eq!(rows[2], "● Checking the threads▅", "{rows:?}");
        assert_eq!(rows[3], "");
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap();
        // The prompt's and the reply's gutters (`❯ `, `● `), then the cursor.
        assert_eq!(
            pane.skip,
            vec![
                Rect::new(pane.area.x, pane.area.y, 2, 1),
                Rect::new(pane.area.x, pane.area.y + 2, 2, 1),
                Rect::new(pane.area.x + 22, pane.area.y + 2, 1, 1)
            ]
        );

        // A finished message: the next text starts below it.
        st.transcript.as_mut().unwrap().entries = vec![prompt("hi"), text("Done.", false)];
        let rows = drawn_rows(&mut st);
        assert_eq!(rows[2..5], ["● Done.", "", "▅"]);

        // A row filled to the edge.
        let width = usize::from(st.view.panes.get(&PaneId::Transcript).unwrap().area.width);
        let full = "x".repeat(width - 2);
        st.transcript.as_mut().unwrap().entries = vec![prompt("hi"), text(&full, true)];
        let rows = drawn_rows(&mut st);
        assert_eq!(rows[2..4], [format!("● {full}").as_str(), "▅"]);
    }

    /// Scrolled up, the conversation stays put: the cursor would be below the view.
    #[test]
    fn a_scrolled_conversation_does_not_move_for_the_cursor() {
        let many: Vec<Entry> = (0..60)
            .map(|i| Entry::Notice {
                seq: 0,
                level: NoticeLevel::Info,
                text: format!("line {i}"),
            })
            .collect();
        let mut st = orch_working(many.clone());
        st.view.scroll = 5;
        let working = draw_panes(&mut st, 120, 30);
        let max = st.view.panes.get(&PaneId::Transcript).unwrap().max_scroll;
        // No cursor cell to leave out of a copy (the notices' marks are).
        assert!(!st
            .view
            .panes
            .get(&PaneId::Transcript)
            .unwrap()
            .skip
            .iter()
            .any(|r| r.width == 1));
        assert!(!transcript_rows(&st, &working)
            .iter()
            .any(|l| l.contains('▅')));

        let mut idle = crate::app::tests::without_worker_requests(crate::app::tests::with_orch());
        idle.transcript.as_mut().unwrap().entries = many;
        idle.view.scroll = 5;
        let still = draw_panes(&mut idle, 120, 30);
        assert_eq!(
            transcript_rows(&st, &working),
            transcript_rows(&idle, &still)
        );
        assert_eq!(
            max,
            idle.view.panes.get(&PaneId::Transcript).unwrap().max_scroll
        );

        // Back at the bottom it shows, below the last line.
        st.view.scroll = 0;
        let rows = drawn_rows(&mut st);
        assert_eq!(rows.last().map(String::as_str), Some("▅"));
        assert_eq!(rows[rows.len() - 3], "· line 59");
    }

    /// No cursor while the orchestrator waits on the user or rests; in a dive, the cursor of
    /// the worker dived into, while it works.
    #[test]
    fn the_cursor_shows_only_while_the_shown_thread_works() {
        use crate::app::tests::control;
        let mut st = orch_working(vec![prompt("go")]);
        let mut o = ThreadScript::new("t-orch");
        o.at(300);
        // A stall does not block the turn: the cursor stays, and the orchestrator's box
        // above the conversation is still drawn in full.
        control(&mut st, &[o.raise("st1", AttentionReason::Stalled)]);
        let rows = drawn_rows(&mut st);
        assert!(rows.iter().any(|r| r == "▅"), "{rows:?}");
        assert!(st
            .view
            .panes
            .drawn_on("t-orch")
            .is_some_and(|d| d.attn_id == "st1"));
        control(&mut st, &[o.raise("op1", AttentionReason::Question)]);
        let rows = drawn_rows(&mut st);
        assert!(!rows.iter().any(|r| r.contains('▅')), "waiting: {rows:?}");

        st.dive("t-gamma");
        st.transcript.as_mut().unwrap().entries = vec![prompt("tests")];
        let rows = drawn_rows(&mut st);
        assert_eq!(rows[..3], ["❯ tests", "", "▅"]);
        let mut beta = ThreadScript::new("t-beta");
        beta.at(400);
        control(&mut st, &[beta.raise("pm2", AttentionReason::Permission)]);
        st.dive("t-beta");
        st.transcript.as_mut().unwrap().entries = vec![prompt("deploy")];
        let rows = drawn_rows(&mut st);
        assert!(
            !rows.iter().any(|r| r.contains('▅')),
            "beta waits: {rows:?}"
        );
    }

    #[test]
    fn hub_questions_render_as_the_first_card() {
        use twr_core::event::{AttentionReason, EventBody};
        use twr_core::proto::{StreamItem, StreamKind};
        let mut st = three_threads();
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(30);
        let mut q = h.raise("hq1", AttentionReason::Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = "Which thread should get: deploy?".into();
            p.options = vec!["alpha".into(), "beta".into(), "new_thread".into()];
        }
        st.apply_event(&StreamItem {
            stream: StreamKind::Control,
            gseq: Some(st.last_gseq + 1),
            event: q,
        });
        st.set_focus(Some(HUB_THREAD.into()));
        let lines = draw_panes(&mut st, 160, 40);
        let s = screen(&lines);
        assert!(s.contains("twr asks"), "{s}");
        assert!(s.contains("Which thread should get"), "{s}");
        assert!(s.contains("1 alpha  2 beta  3 new_thread"), "{s}");
        let at = |id: &str| st.view.panes.get(&card(id)).unwrap().area;
        assert!(at(HUB_THREAD).y < at("t-beta").y, "{s}");
        // Narrow: the leftmost card of the row.
        let lines = draw(&st, 90, 30);
        let row = &lines[0];
        assert!(
            col(row, "twr asks").unwrap() < col(row, "beta").unwrap(),
            "{row}"
        );
    }

    #[test]
    fn narrow_terminal_keeps_focused_card_visible() {
        let mut st = three_threads();
        st.set_focus(Some("t-gamma".into()));
        let lines = draw(&st, 60, 24);
        let top = &lines[0];
        assert!(top.contains(" gamma "), "{top}");
        assert!(top.contains(" alpha "));
        assert!(!top.contains(" beta "));
        assert!(screen(&lines).contains("< 1 more"));
        assert_eq!(visible_window(3, Some(2), 60), (1, 2));
        assert_eq!(visible_window(3, Some(0), 60), (0, 2));
        assert_eq!(visible_window(3, None, 10), (0, 1));
    }

    /// The side column shows the cards that fit, keeping the selected one in view, with
    /// `↑ N more` / `↓ N more` on the edge cards.
    #[test]
    fn the_card_column_keeps_the_selected_card_visible() {
        assert_eq!(column_window(&[11, 11, 7], Some(0), 26), (0, 2));
        assert_eq!(column_window(&[11, 11, 7], Some(2), 26), (1, 2));
        assert_eq!(column_window(&[11, 7, 7], None, 26), (0, 3));
        assert_eq!(column_window(&[11, 11], Some(1), 5), (1, 1));
        assert_eq!(column_window(&[], None, 5), (0, 0));
        let mut st = crate::app::tests::five_threads();
        st.set_focus(Some("t-alpha".into()));
        // 17 rows: 16 for the column; beta (7) and alpha (6) fit, then 3 more.
        let s = screen(&draw_panes(&mut st, 160, 17));
        assert!(s.contains("↓ 3 more┛"), "{s}");
        assert!(!s.contains(" more┐") && !s.contains("↑ 3"), "{s}");
        st.set_focus(Some("t-eps".into()));
        let s = screen(&draw_panes(&mut st, 160, 17));
        assert!(s.contains("┏ proj · eps "), "{s}");
        assert!(s.contains("↑ 3 more"), "{s}");
    }

    #[test]
    fn transcript_rendering() {
        let mut st = three_threads();
        st.dive("t-beta");
        let mut s = ThreadScript::new("t-beta");
        let evs = vec![
            s.created("beta"),
            s.turn_started("deploy it"),
            s.event(EventBody::AgentText(AgentText {
                text: "Deploying ".into(),
                delta: true,
                ..Default::default()
            })),
            s.event(EventBody::AgentText(AgentText {
                text: "now".into(),
                delta: true,
                ..Default::default()
            })),
            s.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: "c1".into(),
                tool: "Bash".into(),
                input_summary: "make deploy".into(),
                ..Default::default()
            })),
            s.event(EventBody::AgentToolResult(AgentToolResult {
                call_id: "c1".into(),
                ok: false,
                ..Default::default()
            })),
            s.turn_ended(TurnResult::Error),
        ];
        for ev in &evs {
            st.apply_event(&thread_item(ev));
        }
        let text = screen(&draw(&st, 160, 30));
        assert!(
            text.starts_with(
                " ‹ back  talking to beta  fake · default model · mode manual · Esc returns to \
                 the orchestrator "
            ),
            "{text}"
        );
        // Narrower, the header keeps what fits: who it talks to, then how to get back.
        let text = screen(&draw(&st, 120, 30));
        assert!(
            text.starts_with(
                " ‹ back  talking to beta  fake · default model · mode manual · Esc: back "
            ),
            "{text}"
        );
        let text = screen(&draw(&st, 100, 30));
        assert!(
            text.starts_with(" ‹ back  talking to beta  mode manual · Esc: back "),
            "{text}"
        );
        let text = screen(&draw(&st, 40, 30));
        assert!(
            text.contains(" ‹ back  talking to beta  Esc: back "),
            "{text}"
        );
        let lines = draw(&st, 12, 30);
        assert!(
            lines.iter().any(|l| l == " talking t… "),
            "{}",
            screen(&lines)
        );
        assert!(text.contains("❯ deploy it"), "{text}");
        assert!(text.contains("● Deploying now"));
        // A dive folds the thread's tool calls like the orchestrator conversation; Ctrl-O
        // shows them.
        let text = screen(&draw(&st, 160, 30));
        assert!(
            text.contains("  Ran 1 shell command, 1 failed (ctrl+o to expand)"),
            "{text}"
        );
        assert!(!text.contains("Bash(make deploy)"), "{text}");
        assert!(text.contains("✗ The turn failed"));
        st.view.expand_all = true;
        let text = screen(&draw(&st, 160, 30));
        assert!(text.contains("● Bash(make deploy)"), "{text}");
        assert!(text.contains("⎿  failed"), "{text}");
    }

    /// Agent-controlled text (thread names, prompts, replies, tool summaries, attention
    /// summaries and options) never reaches the terminal with control characters: ratatui
    /// drops every grapheme containing one when it fills the buffer the terminal is drawn from.
    #[test]
    fn control_characters_never_reach_the_screen() {
        use twr_core::event::AttentionReason;
        let hostile = "x\u{1b}]0;pwned\u{7}\u{1b}[2J\u{9b}31m\r\u{8}\u{7f}y";
        let mut st = AppState::new();
        let mut s = ThreadScript::new("t-evil");
        let mut evs = vec![
            s.created(&format!("ev{hostile}il")),
            s.turn_started(hostile),
            s.text(hostile),
            s.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: "c1".into(),
                tool: hostile.into(),
                input_summary: hostile.into(),
                ..Default::default()
            })),
        ];
        let mut raised = s.raise("q1", AttentionReason::Question);
        if let EventBody::AttentionRaised(p) = &mut raised.body {
            p.summary = hostile.into();
            p.options = vec![hostile.into(), format!("no{hostile}")];
        }
        evs.push(raised);
        crate::app::tests::control(&mut st, &evs);
        for ev in &evs {
            st.apply_event(&thread_item(ev));
        }
        st.view.status = Some(hostile.into());
        for screen in [Screen::Stack, Screen::Grid] {
            st.view.screen = screen;
            let buf = draw_buf(&st, 120, 40);
            let text: String = buf.content().iter().map(|c| c.symbol()).collect();
            assert!(text.contains("pwned"), "the text is shown: {text}");
            assert!(
                !text.chars().any(char::is_control),
                "{screen:?}: {:?}",
                text.chars().filter(|c| c.is_control()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn transcript_scrolls_from_the_bottom() {
        let mut st = three_threads();
        st.dive("t-alpha");
        let mut s = ThreadScript::new("t-alpha");
        st.apply_event(&thread_item(&s.created("alpha")));
        for i in 0..40 {
            st.apply_event(&thread_item(&s.text(&format!("line-{i:02}"))));
        }
        // A 30-row screen leaves 24 inner transcript rows to a dive this narrow (no cards):
        // under the header band, above the bottom border, the input box and the status bar.
        let tail = screen(&draw(&st, 80, 30));
        assert!(tail.contains("line-39") && !tail.contains("line-10"));
        st.handle_key(key(KeyCode::PageUp));
        let up = screen(&draw(&st, 80, 30));
        assert!(up.contains("line-29") && !up.contains("line-39"), "{up}");
        // The bottom border says how much is below and how to get there.
        assert!(up.contains("↓ 10 lines below · End to follow"), "{up}");
        assert!(!tail.contains("below"), "{tail}");
        draw_panes(&mut st, 80, 30);
        assert_eq!(
            st.view.panes.get(&PaneId::Transcript).unwrap().area.height,
            24
        );
    }

    #[test]
    fn status_bar_shows_errors_and_hints() {
        let mut st = three_threads();
        st.view.status = Some("hub: not_found".into());
        let lines = draw(&st, 160, 24);
        let last = lines.last().unwrap();
        assert!(
            last.starts_with("hub: not_found"),
            "no mode badge before it: {last}"
        );
        assert!(last.contains("| Enter send · "), "{last}");
        st.view.status = None;
        let last = draw(&st, 160, 24).pop().unwrap();
        assert!(last.starts_with("Enter send · "), "{last}");
    }

    /// `st` with what the hub knows of the subscriptions' usage: Claude's 5-hour and weekly
    /// windows (`five` and `week` used, read a minute ago) and Codex's weekly one (42%, read 12
    /// minutes ago).
    fn with_usage(mut st: AppState, five: f64, week: f64) -> AppState {
        use twr_core::event::{AccountUsage, Event, EventBody, Source};
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
        st
    }

    /// The subscriptions' remaining usage starts the status bar's last row, before the
    /// hints, and gets shorter as the screen narrows: in full with the data's age, without
    /// it, compact, then gone, so the hints always keep their room.
    #[test]
    fn usage_in_the_status_bar_shortens_with_the_width() {
        let st = with_usage(three_threads(), 22.0, 44.0);
        let hints = hints(&st);
        let head: String = hints.chars().take(10).collect();
        for (w, usage) in [
            (
                240,
                Some("left: Claude 5h 78% · wk 56% | GPT wk 58% as of 12m ago"),
            ),
            (230, Some("left: Claude 5h 78% · wk 56% | GPT wk 58%")),
            (120, Some("left: Claude 5h 78% · wk 56% | GPT wk 58%")),
            (80, Some("C 78%/56% | G 58%")),
            (40, None),
            (30, None),
        ] {
            let last = draw(&st, w, 24).pop().unwrap();
            match usage {
                Some(u) => {
                    assert!(last.starts_with(u), "{w}: {last}");
                    let following = last.strip_prefix(u).unwrap();
                    assert!(
                        following.starts_with(&format!(" {head}")),
                        "hints follow the usage with a gap: {last}"
                    );
                    assert!(
                        crate::edit::width(following.trim()) >= HINTS_MIN,
                        "hints keep their minimum width: {w}: {last}"
                    );
                }
                None => {
                    assert!(last.starts_with(&head), "{w}: {last}");
                    assert!(
                        !last.contains("78%") && !last.contains("left:"),
                        "{w}: {last}"
                    )
                }
            }
        }
        // Nothing known: the hints alone, as before.
        let last = draw(&three_threads(), 120, 24).pop().unwrap();
        assert!(!last.contains("left:") && !last.contains('%'), "{last}");
    }

    /// A busy machine is named in the status bar with what presses; a calm one isn't.
    #[test]
    fn the_status_bar_names_a_busy_machine() {
        use twr_core::load::Load;
        let mut st = three_threads();
        let psi = |v: f64| format!("some avg10={v:.2} avg60=0 avg300=0 total=0\n");
        st.load = Load::from_proc(Some(&psi(3.0)), Some(&psi(0.0)), None, None, 4);
        let screen = draw(&st, 120, 24).join("\n");
        assert!(
            !screen.contains("calm") && !screen.contains("cpu 3%"),
            "{screen}"
        );

        st.load = Load::from_proc(Some(&psi(52.0)), Some(&psi(0.0)), None, None, 4);
        let last = draw(&st, 120, 24).pop().unwrap();
        assert!(last.contains("busy: cpu 52% · "), "{last}");

        st.load = Load::from_proc(Some(&psi(97.0)), Some(&psi(31.0)), None, None, 4);
        let last = draw(&st, 160, 24).pop().unwrap();
        assert!(last.contains("saturated: cpu 97% mem 31% · "), "{last}");
        // A box low on disk is named after it.
        st.disk_low = vec![("local".into(), 4 << 30)];
        let last = draw(&st, 200, 24).pop().unwrap();
        assert!(
            last.contains("saturated: cpu 97% mem 31% · disk low: local 4.0 GB free · "),
            "{last}"
        );
        // The bar keeps its hints after it.
        assert!(last.contains("Enter send"), "{last}");
    }

    #[test]
    fn usage_starts_the_last_row_beside_status_notes() {
        let mut st = with_usage(three_threads(), 22.0, 44.0);
        st.view.status = Some("hub: not_found".into());
        let last = draw(&st, 280, 24).pop().unwrap();
        assert!(last.starts_with("left: Claude 5h 78%"), "{last}");
        assert!(last.contains("ago hub: not_found | Enter send"), "{last}");

        // Wrapped status notes still leave the usage at the start of the last row.
        st.view.status = Some("a long status message ".repeat(8));
        let last = draw(&st, 120, 24).pop().unwrap();
        assert!(last.starts_with("C 78%/56% | G 58% "), "{last}");
        assert!(last.contains("Enter send"), "{last}");
    }

    #[test]
    fn usage_rendering_clamps_remaining_percentages() {
        let st = with_usage(three_threads(), 104.4, -3.0);
        let last = draw(&st, 280, 24).pop().unwrap();
        assert!(last.starts_with("left: Claude 5h 0% resets "), "{last}");
        assert!(last.contains("wk 100%"), "{last}");
        let last = draw(&st, 80, 24).pop().unwrap();
        assert!(last.starts_with("C 0%/100% | G 58% Enter send"), "{last}");
    }

    /// Short hints (a completion popup's) leave the usage more room than two fifths of the row,
    /// and a form that fits beside all the hints wins over a wider one that would cut them.
    #[test]
    fn usage_takes_what_short_hints_leave() {
        let st = with_usage(three_threads(), 22.0, 44.0);
        // Long hints, 100 columns: two fifths of the row, the compact form.
        let parts = usage_spans(&st, 94, 200, chrono::Utc::now(), &chrono::Utc).unwrap();
        let text: String = parts.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "C 78%/56% | G 58%");
        // Hints of 30 cells: the rest is the usage's.
        let parts = usage_spans(&st, 94, 30, chrono::Utc::now(), &chrono::Utc).unwrap();
        let text: String = parts.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(
            text,
            "left: Claude 5h 78% · wk 56% | GPT wk 58% as of 12m ago"
        );
        // 150 columns of 130-cell hints: two fifths would take the full form, but the compact
        // one fits beside every hint.
        let parts = usage_spans(&st, 150, 130, chrono::Utc::now(), &chrono::Utc).unwrap();
        let text: String = parts.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "C 78%/56% | G 58%");
        // 160 columns of 116-cell hints: the brief form fits beside them.
        let parts = usage_spans(&st, 160, 116, chrono::Utc::now(), &chrono::Utc).unwrap();
        let text: String = parts.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "left: Claude 5h 78% · wk 56% | GPT wk 58%");
        // The compact form is 17 cells: a gap and 30 cells of hints make 48.
        assert!(usage_spans(&st, 47, 200, chrono::Utc::now(), &chrono::Utc).is_none());
        assert!(usage_spans(&st, 48, 200, chrono::Utc::now(), &chrono::Utc).is_some());
        // Hints shorter than the minimum keep all their cells, even on a small row.
        assert!(usage_spans(&st, 22, 4, chrono::Utc::now(), &chrono::Utc).is_some());
    }

    /// A window with 25% left or less is yellow and one with 10% or less red and bold, with
    /// its reset time in the full form; data ten minutes old is grey; a question waiting for
    /// `y` hides the usage like the hints.
    #[test]
    fn usage_colors_windows_near_their_limit() {
        let st = with_usage(three_threads(), 90.0, 75.0);
        let buf = draw_buf(&st, 280, 24);
        let y = 23;
        let row: String = (0..280).map(|x| buf[(x, y)].symbol()).collect();
        let at = |needle: &str| {
            let byte = row
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} in {row}"));
            u16::try_from(row[..byte].chars().count()).unwrap()
        };
        assert!(row.contains("5h 10% resets "), "{row}");
        let five = &buf[(at("5h 10%"), y)];
        assert_eq!(five.fg, Color::Red);
        assert!(five.modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(at("wk 25%"), y)].fg, Color::Yellow);
        assert_eq!(buf[(at("wk 58%"), y)].fg, Color::DarkGray, "stale");
        assert!(
            !row.contains("wk 25% resets"),
            "only low windows say when they reset"
        );
        assert!(!five.modifier.contains(Modifier::DIM));

        // The same windows, read twelve minutes ago: dimmed, not bold, in every form.
        let stale = {
            use twr_core::event::{AccountUsage, Event, EventBody, Source};
            let mut st = st.clone();
            let now = chrono::Utc::now();
            let mut ev = Event::new(
                HUB_THREAD,
                1,
                None,
                Source::Hub,
                EventBody::AccountUsage(AccountUsage {
                    runtime: "claude-code".into(),
                    windows: vec![
                        twr_core::usage::UsageWindow {
                            window: "five_hour".into(),
                            used_pct: 90.0,
                            resets_at: Some(now + chrono::TimeDelta::hours(3)),
                            ..Default::default()
                        },
                        twr_core::usage::UsageWindow {
                            window: "seven_day".into(),
                            used_pct: 75.0,
                            resets_at: Some(now + chrono::TimeDelta::hours(100)),
                            ..Default::default()
                        },
                    ],
                    source: "event".into(),
                    ..Default::default()
                }),
            );
            ev.ts = now - chrono::TimeDelta::minutes(12);
            st.apply_control(&ev);
            st
        };
        for w in [220u16, 80] {
            let buf = draw_buf(&stale, w, 24);
            let row: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            let byte = row.find("10%").unwrap_or_else(|| panic!("10% in {row}"));
            let x = u16::try_from(row[..byte].chars().count()).unwrap();
            let cell = &buf[(x, y)];
            assert_eq!(cell.fg, Color::Red, "{w}");
            assert!(cell.modifier.contains(Modifier::DIM), "{w}");
            assert!(!cell.modifier.contains(Modifier::BOLD), "{w}");
            let byte = row.find("25%").unwrap();
            let x = u16::try_from(row[..byte].chars().count()).unwrap();
            assert_eq!(buf[(x, y)].fg, Color::Yellow, "{w}");
            assert!(buf[(x, y)].modifier.contains(Modifier::DIM), "{w}");
        }

        let mut st = st;
        st.view.confirm = Some(crate::app::Confirm {
            action: crate::app::Action::ResetOrchestrator,
            prompt: "reset the orchestrator? y/n".into(),
            pending: String::new(),
            alt: None,
        });
        let last = draw(&st, 220, 24).pop().unwrap();
        assert!(!last.contains("left:"), "{last}");
    }

    fn draw_buf(state: &AppState, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            render(f, state);
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    fn grid_state() -> AppState {
        let mut st = five_threads();
        st.view.screen = Screen::Grid;
        st
    }

    /// Tiles whose top border starts on `row`.
    fn tiles_on(row: &str) -> usize {
        row.chars().filter(|c| matches!(c, '┌' | '┏')).count()
    }

    #[test]
    fn grid_fits_tiles_per_row_to_the_width() {
        let st = grid_state();
        // Row 0 is the header; tile rows start at 1, 1 + TILE_HEIGHT, ...; the input box and
        // the status bar take the last 4 rows.
        let r = |n: u16| usize::from(1 + n * TILE_HEIGHT);
        let lines = draw(&st, 100, 33);
        assert!(
            lines[0].contains("grid · 5 threads · 1 parked"),
            "{}",
            lines[0]
        );
        assert_eq!(
            [
                tiles_on(&lines[r(0)]),
                tiles_on(&lines[r(1)]),
                tiles_on(&lines[r(2)])
            ],
            [2, 2, 1],
            "{}",
            screen(&lines)
        );
        let lines = draw(&st, 180, 30);
        assert_eq!(
            [tiles_on(&lines[r(0)]), tiles_on(&lines[r(1)])],
            [4, 1],
            "{}",
            screen(&lines)
        );
        // Tile order is grid order: beta, alpha, gamma, eps on the first row, parked delta last.
        let top = &lines[r(0)];
        let at = |n: &str| col(top, &format!(" {n} ")).unwrap();
        assert!(at("beta") < at("alpha") && at("alpha") < at("gamma") && at("gamma") < at("eps"));
        assert!(
            lines[r(1)].starts_with("┌ proj · delta "),
            "{}",
            lines[r(1)]
        );
        // No transcript in the grid; the input box and the status bar stay.
        let s = screen(&lines);
        assert!(!s.contains("transcript"));
        assert!(s.contains("→ orchestrator"), "{s}");
        assert!(lines.last().unwrap().contains("Ctrl-G stack"), "{s}");
    }

    /// A card's title leads with the thread's project, dim (`twr · scroll-follow`), where it
    /// used to take a row of its own. Narrow, the permission mode keeps its place: the project
    /// is cut with `…` to the room left, then left out, and the name stays whole.
    #[test]
    fn the_project_leads_the_title_and_gives_way_first() {
        assert_eq!(title_project(Some("twr"), 20).as_deref(), Some("twr"));
        assert_eq!(
            title_project(Some("orbit-webapp"), 9).as_deref(),
            Some("orbit…")
        );
        assert_eq!(
            title_project(Some("orbit-webapp"), 5),
            None,
            "under 2 letters"
        );
        assert_eq!(title_project(Some("  "), 20), None);
        assert_eq!(title_project(None, 20), None);
        let mut st = three_threads();
        let mut p = ThreadScript::new("t-long");
        let mut created = p.created("scroll-follow");
        if let EventBody::ThreadCreated(c) = &mut created.body {
            c.project = Some("orbit-webapp".into());
        }
        crate::app::tests::control(&mut st, &[created]);
        st.set_focus(Some("t-long".into()));
        let title = |st: &mut AppState, w: u16| {
            let lines = draw_panes(st, w, 40);
            let text = pane_text(st, &lines, &card("t-long"));
            text.lines().next().unwrap().to_string()
        };
        // Beside the conversation it fits whole; in the row above it, the narrower the card,
        // the shorter, until the name and the mode leave it no room.
        for (w, project) in [
            (160, Some("orbit-webapp")),
            (99, Some("or…")),
            (36, Some("orbit…")),
            (30, None),
        ] {
            let t = title(&mut st, w);
            match project {
                Some(p) => assert!(t.contains(&format!("┏ {p} · scroll-follow ")), "{w}: {t}"),
                None => assert!(
                    t.contains("┏ scroll-follow ") && !t.contains("orbit") && !t.contains("or…"),
                    "{w}: {t}"
                ),
            }
            assert!(t.contains(" manual "), "{w}: {t}");
        }
        // The tile's title says it too; its turns go after its git state.
        st.view.screen = Screen::Grid;
        let s = screen(&draw(&st, 180, 30));
        assert!(s.contains(" orbit-webapp · scroll-follow "), "{s}");
        assert!(!s.contains("orbit-webapp · 0 turns"), "{s}");
        assert!(s.contains("no git snapshot · 0 turns"), "{s}");
    }

    /// A tile's turn count follows its git state, which is cut with `…` to leave it room, so
    /// a long branch at a narrow grid width never hides it.
    #[test]
    fn a_tiles_turn_count_survives_a_long_branch() {
        let mut st = three_threads();
        let mut p = ThreadScript::new("t-long");
        crate::app::tests::control(
            &mut st,
            &[
                p.created("long"),
                p.turn_started("one"),
                p.git("twr/a-rather-long-branch-name-for-a-tile", 3),
            ],
        );
        st.set_focus(Some("t-long".into()));
        st.view.screen = Screen::Grid;
        for w in [80, 180] {
            let s = screen(&draw(&st, w, 40));
            let row = s
                .lines()
                .find(|l| l.contains("twr/a-rather"))
                .unwrap_or_else(|| panic!("{s}"));
            assert!(row.contains("… · 1 turn"), "{w}: {row}");
        }
    }

    /// One thread in each state a card shows, each with a report of an earlier turn where it
    /// has one: running, waiting on background work (its turn's interim report), waiting for a
    /// permission, done (success), failed, needing review, parked and stopped; and the hub's
    /// activity for those at work and the failed one.
    fn one_of_each_state() -> AppState {
        use crate::app::tests::control;
        use twr_core::event::{Grade, TaskUpdated};
        let mut st = AppState::new();
        st.view.show_parked = true;
        let report = |s: &mut ThreadScript, turn, grade, text: &str| {
            let mut ev = s.report(Some(turn), grade);
            if let EventBody::ReportReady(p) = &mut ev.body {
                p.summary = text.into();
            }
            ev
        };
        let done = "Tax is now rounded once on the invoice total instead of per line. The 12 \
                    invoice tests pass and the branch is pushed.";
        let names = ["run", "bg", "perm", "ok", "fail", "rev", "park", "stop"];
        for (i, name) in names.iter().enumerate() {
            let mut s = ThreadScript::new(format!("t-{name}"));
            let mut evs = vec![
                s.at(i as i64).created(name),
                s.turn_started("one"),
                s.turn_ended(TurnResult::Completed),
                report(&mut s, 1, Grade::Success, "An earlier turn's report."),
            ];
            match *name {
                "run" => evs.push(s.turn_started("two")),
                "bg" => evs.extend([
                    s.turn_started("two"),
                    s.event(EventBody::TaskUpdated(TaskUpdated {
                        task_id: "t1".into(),
                        running: true,
                        ..Default::default()
                    })),
                    s.turn_ended(TurnResult::Completed),
                    report(&mut s, 2, Grade::Success, "Tests run in the background."),
                ]),
                "perm" => evs.extend([
                    s.turn_started("two"),
                    s.raise("p1", AttentionReason::Permission),
                ]),
                "ok" => evs.extend([
                    s.turn_started("two"),
                    s.turn_ended(TurnResult::Completed),
                    report(&mut s, 2, Grade::Success, done),
                ]),
                "fail" => evs.extend([s.turn_started("two"), s.turn_ended(TurnResult::Error)]),
                "rev" => evs.extend([
                    s.turn_started("two"),
                    s.turn_ended(TurnResult::Completed),
                    report(&mut s, 2, Grade::NeedsVerification, done),
                    s.raise("r1", AttentionReason::Review),
                ]),
                "park" => evs.push(s.parked()),
                "stop" => evs.push(s.stopped()),
                _ => unreachable!(),
            }
            control(&mut st, &evs);
        }
        st.apply_result(
            "thread.activity",
            &serde_json::json!({
                "t-run": {"activity": "running the invoice tests"},
                "t-bg": {"activity": "waiting on background: cargo test --workspace"},
                "t-fail": {"activity": "turn 2 failed: rate limited"},
                "t-ok": {"activity": "done: Tax is now rounded once"},
            }),
        );
        st
    }

    /// One slot: a card's status says its state (with its report's grade once it is
    /// done), and the one slot under its git row says what is going on: an open item, else
    /// what it does while at work or why it failed, else its last report, cut to the rows the
    /// card has. Never the activity and the report both, never a report of an earlier turn
    /// while it works, never a `report:` label. In the column (34, 40 and 48 columns), the row
    /// above a narrow conversation, and the grid.
    #[test]
    fn a_card_shows_one_slot_for_each_state() {
        let expect: [(&str, &str, &[&str], &[&str]); 8] = [
            (
                "run",
                "running",
                &["› running the invoice"],
                &["earlier", "Tax"],
            ),
            (
                "bg",
                "background (1 bg)",
                &["› waiting on background"],
                &["Tests run", "earlier"],
            ),
            ("perm", "waiting", &[" PERMISSION "], &["›", "earlier"]),
            ("ok", "done ✓", &["Tax is now rounded"], &["›", "earlier"]),
            ("fail", "failed", &["› turn 2 failed"], &["earlier"]),
            ("rev", "done ?", &[" REVIEW "], &["›", "earlier"]),
            ("park", "parked", &["An earlier turn's report."], &["›"]),
            ("stop", "stopped", &["An earlier turn's report."], &["›"]),
        ];
        let check = |what: &str, text: &str, name: &str| {
            let (_, status, has, not) = expect.iter().find(|e| e.0 == name).unwrap();
            let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                words.contains(&format!("{status} ·")) || words.contains(&format!("{status} ")),
                "{what} {name}: {status}\n{text}"
            );
            for h in *has {
                assert!(text.contains(h), "{what} {name}: {h}\n{text}");
            }
            for n in *not {
                assert!(!text.contains(n), "{what} {name}: not {n}\n{text}");
            }
            assert!(!text.contains("report:"), "{what} {name}\n{text}");
        };
        for w in [100, 134, 160] {
            let mut st = one_of_each_state();
            let lines = draw_panes(&mut st, w, 80);
            assert_eq!(
                side_width(w) - 2,
                [32, 38, 46][[100, 134, 160].iter().position(|x| *x == w).unwrap()]
            );
            for (name, ..) in &expect {
                let text = pane_text(&st, &lines, &card(&format!("t-{name}")));
                check(&format!("column {w}"), &text, name);
            }
        }
        // The row of cards above a narrow conversation: each card, selected to show it.
        for w in [60, 90] {
            for (name, ..) in &expect {
                let mut st = one_of_each_state();
                st.set_focus(Some(format!("t-{name}")));
                let lines = draw_panes(&mut st, w, 40);
                let text = pane_text(&st, &lines, &card(&format!("t-{name}")));
                if *name == "bg" {
                    // Selected, it shows the report its turn yielded with: an interim one.
                    assert!(text.contains("Tests run in the"), "row {w}:\n{text}");
                    continue;
                }
                check(&format!("row {w}"), &text, name);
            }
        }
        let mut st = one_of_each_state();
        st.view.screen = Screen::Grid;
        let lines = draw_panes(&mut st, 160, 60);
        for (name, ..) in &expect {
            let text = pane_text(&st, &lines, &card(&format!("t-{name}")));
            check("grid", &text, name);
        }
    }

    /// The slot keeps its rows: a long git state takes one row, cut with `…`, so a narrow
    /// column card still says what its thread does.
    #[test]
    fn a_long_git_state_leaves_the_slot_its_rows() {
        use crate::app::tests::control;
        let mut st = AppState::new();
        let mut s = ThreadScript::new("t-long");
        let branch = format!("twr/{}", "a-very-long-branch-name-".repeat(5));
        let evs = [s.created("long"), s.turn_started("go"), s.git(&branch, 3)];
        control(&mut st, &evs);
        st.apply_result(
            "thread.activity",
            &serde_json::json!({"t-long": {"activity": "running the invoice tests"}}),
        );
        for w in [100, 134, 160] {
            let lines = draw_panes(&mut st, w, 40);
            let text = pane_text(&st, &lines, &card("t-long"));
            assert!(text.contains("› running the invoice"), "{w}:\n{text}");
            let git: Vec<&str> = text.lines().filter(|l| l.contains("a-very-long")).collect();
            assert_eq!(git.len(), 1, "{w}:\n{text}");
            assert!(git[0].contains('…'), "{w}:\n{text}");
        }
    }

    /// A tile cut to a row or two (a tiny screen) records its working icon when the icon is
    /// drawn, also on a status row cut short with `…`, and not when it is not.
    #[test]
    fn a_cut_tile_notes_its_icon_when_it_draws_it() {
        let mut st = one_of_each_state();
        st.view.animate = false;
        st.view.screen = Screen::Grid;
        st.set_focus(Some("t-run".into()));
        let mut seen = std::collections::HashSet::new();
        for h in 6..12u16 {
            let lines = draw_panes(&mut st, 40, h);
            let Some(pane) = st.view.panes.get(&card("t-run")).cloned() else {
                continue;
            };
            let text = pane_text(&st, &lines, &card("t-run"));
            // The icon is drawn (`_` with animation off) at the start of its first row.
            let drawn = text.lines().nth(1).is_some_and(|l| l.starts_with("┃_ "));
            assert_eq!(!pane.skip.is_empty(), drawn, "{h}:\n{text}");
            seen.insert(drawn);
        }
        assert!(
            seen.contains(&true),
            "an icon kept on a row cut short is noted"
        );
    }

    /// A model name of wide characters is measured in cells: on a narrow card its row is cut
    /// with `…` within the card, not clipped by the border.
    #[test]
    fn a_wide_model_name_is_cut_in_cells() {
        use crate::app::tests::control;
        use twr_core::event::ThreadCreated;
        let mut st = AppState::new();
        let mut s = ThreadScript::new("t-wide");
        let created = s.event(EventBody::ThreadCreated(ThreadCreated {
            name: "wide".into(),
            runtime: "acp".into(),
            ext: serde_json::json!({"twr": {"model": "模型".repeat(9)}})
                .as_object()
                .cloned(),
            ..Default::default()
        }));
        control(&mut st, &[created]);
        let lines = draw_panes(&mut st, 100, 40);
        let text = pane_text(&st, &lines, &card("t-wide"));
        let row = text
            .lines()
            .find(|l| l.contains("模"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(row.contains('…'), "{text}");
    }

    /// The cells a card's working icon takes out of copied text: the icon and the space after
    /// it, or only the icon when the row is cut right after it (its `…` stays copied).
    #[test]
    fn the_icon_leaves_out_only_the_cells_it_takes() {
        let icon = icon::cursor(None);
        let status = Line::from(vec![icon.clone(), Span::raw(" "), Span::raw("running")]);
        assert_eq!(
            shows_icon(std::slice::from_ref(&status), Some(&status)),
            Some(icon::WIDTH + 1)
        );
        let cut = Line::from(vec![icon.clone(), Span::raw("…")]);
        assert_eq!(shows_icon(&[cut], Some(&status)), Some(icon::WIDTH));
        let gone = Line::from(Span::raw("…"));
        assert_eq!(shows_icon(&[gone], Some(&status)), None);
        assert_eq!(shows_icon(&[status], None), None);
    }

    /// A report known only by its capped preview (its fetch failed) says so in the popup.
    #[test]
    fn the_popup_says_when_it_has_only_a_reports_preview() {
        use crate::app::tests::control;
        use crossterm::event::KeyCode;
        let mut st = AppState::new();
        let mut s = ThreadScript::new("t-cap");
        let mut report = s.report(Some(1), twr_core::event::Grade::NeedsVerification);
        if let EventBody::ReportReady(p) = &mut report.body {
            p.summary = "The start of a long report".into();
            p.extra
                .insert("_capped".into(), serde_json::json!(["summary"]));
        }
        let evs = [
            s.created("cap"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Completed),
            report,
        ];
        control(&mut st, &evs);
        st.set_focus(Some("t-cap".into()));
        st.handle_key(key(KeyCode::Enter));
        // Its own rows: a narrow popup cuts the turn and grade line, never the notice.
        for w in [40, 160] {
            let lines = draw_panes(&mut st, w, 40);
            let popup = pane_text(&st, &lines, &PaneId::Popup);
            assert!(popup.contains("a preview"), "{w}:\n{popup}");
        }
    }

    /// With a filter typed, the report takes the room the rows the filter keeps leave: on a
    /// screen where all the rows leave it none, filtering them to one brings it back.
    #[test]
    fn a_popup_filter_gives_the_report_the_room_it_leaves() {
        use crossterm::event::KeyCode;
        let shown = |h: u16, filter: &str| {
            let mut st = one_of_each_state();
            st.set_focus(Some("t-ok".into()));
            st.handle_key(key(KeyCode::Enter));
            for c in filter.chars() {
                st.handle_key(key(KeyCode::Char(c)));
            }
            let lines = draw_panes(&mut st, 160, h);
            st.view
                .panes
                .get(&PaneId::Popup)
                .map(|_| pane_text(&st, &lines, &PaneId::Popup))
                .unwrap_or_default()
        };
        let h = (8..30)
            .find(|&h| {
                // Room for four rows inside (label, a row of it, the rule, one action),
                // all of them taken by the actions.
                let all = shown(h, "");
                all.lines().count() >= 6 && !all.contains("last report")
            })
            .expect("a height where the rows leave the report no room");
        let filtered = shown(h, "pin");
        assert!(filtered.contains("last report"), "{h}:\n{filtered}");
        // A filter nothing matches leaves no row: the report has the popup's room, whole or
        // cut with `…` (never an empty popup), on any screen with room for a row of it.
        for h in 6..30 {
            let none = shown(h, "zzz");
            // Room for a row: the popup without a filter draws one.
            if shown(h, "").lines().count() >= 3 {
                assert!(none.contains("last report"), "{h}:\n{none}");
                assert!(
                    none.contains("pushed.") || none.contains('…'),
                    "{h}:\n{none}"
                );
            }
        }
    }

    /// The action popup of a card with nothing waiting shows its last report in full, under
    /// a line naming its turn and grade; a card with an item shows the item instead.
    #[test]
    fn the_action_popup_shows_the_last_report_in_full() {
        use crossterm::event::KeyCode;
        let mut st = one_of_each_state();
        st.set_focus(Some("t-ok".into()));
        st.handle_key(key(KeyCode::Enter));
        let lines = draw_panes(&mut st, 160, 50);
        let popup = pane_text(&st, &lines, &PaneId::Popup);
        let words = popup
            .replace('│', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(words.contains("last report · turn 2 · success"), "{popup}");
        assert!(words.contains("the branch is pushed."), "in full: {popup}");
        st.handle_key(key(KeyCode::Esc));
        st.set_focus(Some("t-perm".into()));
        st.handle_key(key(KeyCode::Enter));
        let lines = draw_panes(&mut st, 160, 50);
        let popup = pane_text(&st, &lines, &PaneId::Popup);
        assert!(!popup.contains("last report"), "{popup}");
    }

    /// Cards (and tiles) have a fixed height: a card's activity, report or git state changing
    /// never moves the cards after it, in the column at any width, the row of cards above a
    /// narrow conversation, and the grid; a report too long for its card ends in `…`.
    #[test]
    fn cards_keep_their_height_when_their_text_changes() {
        use crate::app::tests::control;
        use twr_core::event::Grade;
        let report = |s: &mut ThreadScript, turn, text: &str| {
            let mut ev = s.report(Some(turn), Grade::Success);
            if let EventBody::ReportReady(p) = &mut ev.body {
                p.summary = text.into();
            }
            ev
        };
        let areas = |st: &AppState| -> Vec<(String, Rect)> {
            st.view
                .panes
                .panes
                .iter()
                .filter_map(|p| match &p.id {
                    PaneId::Thread(t) => Some((t.clone(), p.area)),
                    _ => None,
                })
                .collect()
        };
        for (w, h, grid) in [
            (100, 60, false),
            (120, 60, false),
            (160, 60, false),
            (90, 40, false),
            (60, 40, false),
            (90, 40, true),
            (160, 40, true),
        ] {
            let mut st = AppState::new();
            let mut s: Vec<ThreadScript> = ["a", "b", "c", "d"]
                .iter()
                .map(|n| ThreadScript::new(format!("t-{n}")))
                .collect();
            for (i, n) in ["a", "b", "c", "d"].iter().enumerate() {
                let evs = [s[i].at(i as i64).created(n), s[i].turn_started("go")];
                control(&mut st, &evs);
            }
            let evs = [
                s[3].turn_ended(TurnResult::Completed),
                report(&mut s[3], 1, "Short."),
            ];
            control(&mut st, &evs);
            if grid {
                st.view.screen = Screen::Grid;
            }
            st.settle_order(std::time::Instant::now());
            draw_panes(&mut st, w, h);
            let before = areas(&st);
            assert!(!before.is_empty(), "{w}x{h}");
            let long = "Rewrote the parser, the lexer and the error messages, ran every \
                        test twice and pushed the branch after a rebase onto main. "
                .repeat(3);
            let evs = [
                s[0].text("Now let me run the whole test suite again."),
                s[1].git("twr/a-much-longer-branch-name-than-before", 7),
                report(&mut s[3], 1, &long),
            ];
            control(&mut st, &evs);
            st.settle_order(std::time::Instant::now());
            let lines = draw_panes(&mut st, w, h);
            assert_eq!(areas(&st), before, "{w}x{h}:\n{}", screen(&lines));
            if !grid && w >= SIDE_MIN_WIDTH {
                let d = pane_text(&st, &lines, &card("t-d"));
                assert!(d.contains("Rewrote the parser"), "{w}x{h}:\n{d}");
                assert!(d.contains('…'), "the report is cut:\n{d}");
                let rows = d.lines().count() as u16;
                assert_eq!(rows, COMPACT_CARD_HEIGHT, "{w}x{h}:\n{d}");
            }
        }
    }

    /// [`three_threads`] plus `rep`, done, whose report the hub wrote when reports ended with
    /// `— twr attach <name>`.
    fn reported_thread() -> AppState {
        use twr_core::event::Grade;
        let mut st = three_threads();
        let mut r = ThreadScript::new("t-rep");
        let mut report = r.report(Some(1), Grade::Success);
        if let EventBody::ReportReady(p) = &mut report.body {
            p.summary = "Fixed the login redirect. All 214 tests pass. — twr attach rep".into();
        }
        let evs = [
            r.created("rep"),
            r.turn_started("fix"),
            r.turn_ended(TurnResult::Completed),
            report,
        ];
        crate::app::tests::control(&mut st, &evs);
        st
    }

    /// A card shows its last report as it is, at every width: no `report:` label before it, no
    /// `— twr attach <name>` after it (a report logged with one shows without it).
    #[test]
    fn a_cards_report_is_the_report_alone() {
        for w in [44, 60, 80, 120, 160] {
            let mut st = reported_thread();
            // Selected, so a narrow row of cards shows it.
            st.set_focus(Some("t-rep".into()));
            let lines = draw_panes(&mut st, w, 50);
            let text = pane_text(&st, &lines, &card("t-rep"));
            let words = flat(&text.lines().map(String::from).collect::<Vec<_>>());
            assert!(words.contains("Fixed the login redirect."), "{w}:\n{text}");
            assert!(!text.contains("report:"), "{w}:\n{text}");
            assert!(!text.contains("twr attach"), "{w}:\n{text}");
            assert!(!words.contains('—'), "{w}:\n{text}");
        }
    }

    /// A grid tile shows the report alone too, at the narrowest tile (40 columns) and wider.
    #[test]
    fn a_tiles_report_is_the_report_alone() {
        for w in [40, 60, 90, 180] {
            let mut st = reported_thread();
            st.set_focus(Some("t-rep".into()));
            st.view.screen = Screen::Grid;
            let s = screen(&draw(&st, w, 40));
            let tile = s
                .lines()
                .skip_while(|l| !l.contains("rep "))
                .take(usize::from(TILE_HEIGHT))
                .collect::<Vec<_>>();
            let words = flat(&tile.iter().map(|l| l.to_string()).collect::<Vec<_>>());
            assert!(words.contains("Fixed the login redirect."), "{w}:\n{s}");
            assert!(!s.contains("report:"), "{w}:\n{s}");
            assert!(!s.contains("twr attach"), "{w}:\n{s}");
        }
    }

    /// A tile says what its thread's turns spent after the turn count.
    #[test]
    fn grid_tiles_say_what_the_turns_spent() {
        let mut st = grid_state();
        let mut z = ThreadScript::new("t-zeta");
        z.at(40);
        let mut evs = vec![
            z.created("zeta"),
            z.turn_started("go"),
            z.turn_ended(twr_core::event::TurnResult::Completed),
        ];
        if let EventBody::TurnEnded(p) = &mut evs[2].body {
            p.usage = Some(twr_core::event::Usage {
                input: 1_150_000,
                output: 50_000,
                cost: Some(3.4),
            });
        }
        crate::app::tests::control(&mut st, &evs);
        let screen = draw(&st, 180, 40).join("\n");
        assert!(screen.contains("1 turn · 1.2M tok $3.40"), "{screen}");
    }

    #[test]
    fn grid_tile_content_and_focus() {
        let mut st = grid_state();
        st.set_focus(Some("t-alpha".into()));
        let lines = draw(&st, 180, 30);
        let tile = |i: usize| -> String {
            lines[1..1 + usize::from(TILE_HEIGHT)]
                .iter()
                .map(|l| l.chars().skip(i * 45).take(45).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let beta = tile(0);
        assert!(beta.contains("waiting · fake@local"), "{beta}");
        assert!(beta.contains(" proj · beta "), "{beta}");
        assert!(beta.contains("no git snapshot · 1 turn"), "{beta}");
        assert!(beta.contains(" PERMISSION  Bash: rm -rf target"));
        // An open item shows its options (what an option key answers) instead of the report.
        assert!(beta.contains("1 allow  2 deny"), "{beta}");
        assert!(!beta.contains("no report yet"), "{beta}");
        let eps = tile(3);
        // Done, with its report's grade; the report takes the rows left, here all of it.
        assert!(eps.contains("done ✓ · fake@local"), "{eps}");
        assert!(eps.contains("twr/eps · changes made · ↑2 ↓1"), "{eps}");
        assert!(
            eps.contains("│Rewrote the README quick start and the"),
            "{eps}"
        );
        assert!(!eps.contains("report:") && !eps.contains('›'), "{eps}");
        assert!(eps.contains("│prose."), "{eps}");
        // alpha is selected: thick border.
        assert_eq!(lines[1].chars().nth(45), Some('┏'), "{}", lines[1]);
        assert_eq!(lines[1].chars().next(), Some('┌'));
    }

    #[test]
    fn parked_threads_are_dimmed_in_both_views() {
        let st = grid_state();
        let buf = draw_buf(&st, 100, 33);
        // delta is alone on the third tile row (y = 1 + 2 * TILE_HEIGHT), name at x = 9, after
        // its project (`proj · `).
        let y = 1 + 2 * TILE_HEIGHT;
        assert_eq!(buf[(9, y)].symbol(), "d");
        assert!(buf[(9, y)].modifier.contains(Modifier::DIM));
        assert_eq!(buf[(2, y)].symbol(), "p");
        assert!(buf[(2, y)].modifier.contains(Modifier::DIM));
        assert!(buf[(2, y + 1)].modifier.contains(Modifier::DIM), "body too");
        // beta is not.
        assert_eq!(buf[(9, 1)].symbol(), "b");
        assert!(!buf[(9, 1)].modifier.contains(Modifier::DIM));

        // Stack view at 180: beta, alpha, delta, gamma, eps (36 columns each).
        let mut st = st;
        // A row of cards at 90 columns: beta, alpha, delta (30 columns each).
        st.view.screen = Screen::Stack;
        let buf = draw_buf(&st, 90, 30);
        assert_eq!(buf[(9 + 2 * 30, 0)].symbol(), "d");
        assert!(buf[(9 + 2 * 30, 0)].modifier.contains(Modifier::DIM));
        assert!(!buf[(9 + 30, 0)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn git_info_on_stack_cards() {
        let mut st = five_threads();
        st.set_focus(Some("t-eps".into()));
        let lines = draw_panes(&mut st, 180, 60);
        let card = pane_text(&st, &lines, &card("t-eps"));
        assert!(card.contains(" eps "), "{card}");
        assert!(card.contains("twr/eps · changes made · ↑2 ↓1"), "{card}");
        assert!(!card.contains("dirty"), "{card}");
        // Cards without a snapshot show no git line.
        assert!(!screen(&lines).contains("no git snapshot"));

        let g = |branch: Option<&str>, dirty, ahead, behind| {
            git_text(&GitSnapshot {
                branch: branch.map(str::to_string),
                dirty_files: dirty,
                ahead,
                behind,
                ..Default::default()
            })
        };
        assert_eq!(g(Some("main"), 0, 0, 0), "main · ↑0 ↓0");
        assert_eq!(g(None, 1, 0, 4), "(detached) · changes made · ↑0 ↓4");
        assert_eq!(g(Some("main"), 12, 3, 0), "main · changes made · ↑3 ↓0");
        // Against its base, when it has one.
        let based = GitSnapshot {
            branch: Some("twr/api".into()),
            base: Some("refs/heads/dev".into()),
            base_ahead: Some(4),
            base_behind: Some(12),
            conflicts: Some(vec!["hub.rs".into(), "lib.rs".into()]),
            ..Default::default()
        };
        assert_eq!(git_text(&based), "twr/api · ↑4 ↓12 dev");
        assert_eq!(conflicts_text(&based), "conflicts: 2 files · ");
        let line: String = git_line(&based)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(line, "conflicts: 2 files · twr/api · ↑4 ↓12 dev");
        let clean = GitSnapshot {
            conflicts: Some(vec![]),
            conflicted: Some(false),
            ..based.clone()
        };
        assert_eq!(conflicts_text(&clean), "");
        let nameless = GitSnapshot {
            conflicts: Some(vec![]),
            conflicted: Some(true),
            ..based
        };
        assert_eq!(conflicts_text(&nameless), "conflicts · ");
        assert_eq!(short_ref("refs/remotes/origin/main"), "origin/main");
        assert_eq!(short_ref("refs/tags/v1"), "tags/v1");
        assert_eq!(short_ref(&"a".repeat(40)), "a".repeat(10));
    }

    /// Hidden parked threads are counted: a dim row under the side column of cards, the
    /// head of the hints when the cards are a row, the grid's header, the empty pane.
    #[test]
    fn hidden_parked_threads_are_counted_where_the_cards_are() {
        use crate::app::tests::{control, with_parked_zeta};
        let mut st = with_parked_zeta();
        let note = "1 parked hidden · /parked shows them";
        let lines = draw(&st, 160, 40);
        assert!(!screen(&lines).contains(" zeta "), "{}", screen(&lines));
        let row = lines
            .iter()
            .position(|l| l.contains(note))
            .unwrap_or_else(|| panic!("{}", screen(&lines)));
        assert!(row < 39, "above the status bar");
        assert!(!lines[39].contains(note));
        // Cards in a row: the hints say it.
        let lines = draw(&st, 90, 30);
        assert!(lines[29].contains(note), "{}", lines[29]);
        assert_eq!(screen(&lines).matches(note).count(), 1);
        // A long status wraps above it, in full.
        let long = "the delete card for web is waiting for you; ".repeat(4);
        st.view.status = Some(long.clone());
        let lines = draw(&st, 90, 30);
        let bar = flat(&lines[26..]);
        assert!(bar.contains(long.trim()), "{bar}");
        assert!(bar.contains(note), "{bar}");
        assert_eq!(screen(&lines).matches("/parked shows them").count(), 1);
        // Wide characters count as two cells.
        let wide = "削除カードが待っています".repeat(4);
        st.view.status = Some(wide.clone());
        let lines = draw(&st, 90, 30);
        let bar = flat(&lines[26..]);
        // (A wide character's second cell reads as a space.)
        assert!(
            bar.contains(note) && bar.replace(' ', "").contains(&wide),
            "{bar}"
        );
        st.view.status = None;
        // The grid's header: delta (parked, with an open item) shows, zeta does not.
        st.view.screen = Screen::Grid;
        let lines = draw(&st, 160, 40);
        assert!(
            lines[0].contains("· 2 parked (1 hidden, /parked shows)"),
            "{}",
            lines[0]
        );
        // Listed: no note.
        st.view.show_parked = true;
        let lines = draw(&st, 160, 40);
        assert!(lines[0].contains("· 2 parked") && !lines[0].contains("hidden"));
        st.view.screen = Screen::Stack;
        let s = screen(&draw(&st, 160, 64));
        assert!(s.contains(" zeta ") && !s.contains("parked hidden"), "{s}");
        // Nothing else: the empty pane says it.
        let mut st = AppState::new();
        let mut z = ThreadScript::new("t-zeta");
        control(&mut st, &[z.created("zeta"), z.parked()]);
        let s = screen(&draw(&st, 160, 40));
        assert!(s.contains("no worker threads") && s.contains(note), "{s}");
        assert_eq!(s.matches(note).count(), 1, "{s}");
        // Narrow, the empty row has no room for it: the hints say it, once.
        let lines = draw(&st, 90, 30);
        let s = screen(&lines);
        assert!(s.contains("no worker threads"), "{s}");
        assert!(lines[29].contains(note), "{s}");
        assert_eq!(s.matches(note).count(), 1, "{s}");
        st.view.screen = Screen::Grid;
        let lines = draw(&st, 160, 40);
        let s = screen(&lines);
        assert!(s.contains(note) && !lines[39].contains(note), "{s}");
        assert_eq!(s.matches(note).count(), 1, "{s}");
        // A grid header too narrow for its `hidden` part: the hints say it.
        let mut st = with_parked_zeta();
        st.view.screen = Screen::Grid;
        let lines = draw(&st, 44, 30);
        assert!(!lines[0].contains("H shows"), "{}", lines[0]);
        assert!(lines[29].contains(note), "{}", screen(&lines));
    }

    #[test]
    fn grid_scrolls_to_keep_the_focused_tile_visible() {
        let mut st = grid_state();
        // 17 rows: 16 for the grid, 15 below the header: one tile row fits.
        let lines = draw(&st, 100, 17);
        assert!(lines[1].contains(" beta ") && !screen(&lines).contains(" delta "));
        assert!(lines[0].contains("↓ 2 more rows"), "{}", lines[0]);
        st.set_focus(Some("t-delta".into()));
        let lines = draw(&st, 100, 17);
        assert!(
            lines[1].starts_with("┏ proj · delta "),
            "{}",
            screen(&lines)
        );
        assert!(lines[0].contains("↑ 2 more rows"), "{}", lines[0]);
        assert!(!lines[0].contains('↓'));
        assert_eq!(grid_first_row(2, 1), 2);
        assert_eq!(grid_first_row(1, 3), 0);
        assert_eq!(grid_first_row(4, 0), 4);
    }

    #[test]
    fn hub_questions_are_the_first_tile() {
        use twr_core::proto::{StreamItem, StreamKind};
        let mut st = grid_state();
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(40);
        let mut q = h.raise("hq1", AttentionReason::Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = "Which thread should get: deploy?".into();
            p.options = vec!["alpha".into(), "beta".into()];
        }
        st.apply_event(&StreamItem {
            stream: StreamKind::Control,
            gseq: Some(st.last_gseq + 1),
            event: q,
        });
        let lines = draw(&st, 180, 30);
        assert!(lines[1].starts_with("┌ twr asks "), "{}", lines[1]);
        let s = screen(&lines);
        assert!(s.contains("Which thread should get"), "{s}");
        assert!(s.contains("1 alpha  2 beta"), "{s}");
        // Still 5 threads; the hub card is not one.
        assert!(lines[0].contains("5 threads"));
        assert_eq!(tiles_on(&lines[1]), 4);
    }

    #[test]
    fn key_help_follows_the_view_and_confirmation_takes_the_status_bar() {
        let mut st = five_threads();
        let last = |st: &AppState| draw(st, 180, 30).pop().unwrap();
        let l = last(&st);
        assert!(
            l.contains("Ctrl-G grid") && l.contains("Enter on an empty line: actions"),
            "{l}"
        );
        st.handle_key(crate::app::tests::ctrl('g'));
        let l = last(&st);
        assert!(
            l.contains("Shift-arrows/Tab select") && l.contains("Ctrl-G stack"),
            "{l}"
        );
        st.handle_key(crate::app::tests::ctrl('g'));
        // The action popup's own keys, then a confirmation in the status bar.
        st.set_focus(Some("t-alpha".into()));
        st.handle_key(key(KeyCode::Enter));
        let l = last(&st);
        assert!(
            l.contains("1-9 answer · ↑↓/tap pick · Enter/tap again run"),
            "{l}"
        );
        for c in "park".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        draw_panes(&mut st, 180, 30);
        st.handle_key(key(KeyCode::Enter));
        let l = last(&st);
        assert!(
            l.contains("park alpha? y to confirm, any other key cancels"),
            "{l}"
        );
        assert!(!l.contains("Ctrl-G"), "the prompt replaces the help: {l}");
        st.handle_key(key(KeyCode::Char('y')));
        // The hub's answer (or error) lands in the status bar.
        st.view.status = Some("thread.park: unsupported: not implemented".into());
        let l = last(&st);
        assert!(l.contains("thread.park: unsupported"), "{l}");
    }

    #[test]
    fn help_overlay_lists_the_commands_and_keys() {
        let mut st = three_threads();
        let l = draw(&st, 200, 40).pop().unwrap();
        assert!(l.contains("? help"), "{l}");
        st.handle_key(key(KeyCode::Esc));
        let l = draw(&st, 200, 40).pop().unwrap();
        assert!(l.contains("/ commands"), "{l}");
        for c in "/help".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        st.handle_key(key(KeyCode::Enter));
        let s = help_text(&mut st, 130, 60);
        for part in [
            "/orch [<runtime> [model]]",
            "switch the orchestrator (y: with a summary, s: without)",
            "/open [thread]",
            "/back",
            "/mode [thread] <mode>",
            "/model <thread> [model] [--effort e]",
            "Ctrl-O",
            "Ctrl-B",
            "Ctrl-F",
            "/sidebar [on|off]",
            "/focus [on|off]",
            "alias /orchestrator",
            "Ctrl-U",
            "/new <project> [--runtime r] [--name n]",
            "/park [thread]",
            "/resume [thread]",
            "/stop [thread]",
            "/attach [thread]",
            "`twr attach` command",
            "//text",
            "Keys",
            "Keys (typing)",
            "Shift-←, Shift-→",
            "Ctrl-↑↓, Alt-↑↓",
            "/grid",
            "/decisions",
            "/pin [thread]",
            "/quit",
        ] {
            assert!(s.contains(part), "{part:?} missing:\n{s}");
        }
        // Closed by a key that does not scroll it: the view is back.
        st.handle_key(key(KeyCode::Char('z')));
        assert!(!screen(&draw(&st, 120, 40)).contains("twr help"));
        // A small terminal still draws.
        st.view.help = true;
        draw(&st, 30, 8);
    }

    /// The help overlay's text, all of it, as one line of words: scrolled from its top to its
    /// end a page at a time on a `w` x `h` screen, each row of it taken once. Also checks that
    /// its bottom border says `↓ more` exactly while there is more below.
    fn help_text(st: &mut AppState, w: u16, h: u16) -> String {
        st.handle_key(key(KeyCode::Home));
        let mut rows = std::collections::BTreeMap::new();
        loop {
            let lines = draw_panes(st, w, h);
            let area = st.view.panes.get(&PaneId::Help).expect("the help").area;
            let max = st.view.panes.help_max;
            let scroll = st.view.help_scroll.min(max);
            for (i, y) in (area.y..area.bottom()).enumerate() {
                let row: String = lines[usize::from(y)]
                    .chars()
                    .skip(usize::from(area.x))
                    .take(usize::from(area.width))
                    .collect();
                rows.insert(usize::from(scroll) + i, row);
            }
            let border = &lines[usize::from(area.bottom())];
            assert_eq!(border.contains("↓ more"), scroll < max, "{w}x{h}: {border}");
            if scroll == max {
                break;
            }
            for _ in 0..area.height {
                st.handle_key(key(KeyCode::Down));
            }
        }
        flat(&rows.into_values().collect::<Vec<_>>())
    }

    /// The help overlay wraps to the screen's width and scrolls, so every command and key
    /// shows whole on any screen: on a phone, at 80x24, on a wide terminal. Below 70 columns
    /// each command or key has a row of its own, what it does indented under it; from 70 on,
    /// a table's first column is as wide as its longest entry. ↑↓, PgUp/PgDn, Home/End and
    /// the wheel scroll it; Esc or `q` closes it.
    #[test]
    fn the_help_wraps_and_scrolls_on_any_screen() {
        let mut entries: Vec<(&str, &str)> = crate::slash::COMMANDS
            .iter()
            .map(|(_, usage, what, _)| (*usage, *what))
            .collect();
        entries.extend_from_slice(HELP_KEYS);
        entries.extend_from_slice(HELP_EDIT_KEYS);
        entries.extend_from_slice(HELP_MOUSE);
        entries.extend_from_slice(HELP_PHONE);
        let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        for (w, h) in [(30, 20), (40, 20), (45, 30), (80, 24), (120, 40), (200, 60)] {
            let mut st = three_threads();
            st.handle_key(key(KeyCode::Char('?')));
            let text = help_text(&mut st, w, h);
            for (k, what) in &entries {
                let entry = format!("{} {}", words(k), words(what));
                assert!(text.contains(&entry), "{w}x{h}: {entry:?} missing:\n{text}");
            }
            // Stacked below 70 columns, a table from 70 on.
            st.handle_key(key(KeyCode::Home));
            let lines = draw_panes(&mut st, w, h);
            let area = st.view.panes.get(&PaneId::Help).unwrap().area;
            let rows: Vec<String> = lines[usize::from(area.y)..usize::from(area.bottom())]
                .iter()
                .map(|l| {
                    l.chars()
                        .skip(usize::from(area.x))
                        .take(usize::from(area.width))
                        .collect()
                })
                .collect();
            let open = rows
                .iter()
                .position(|r| r.trim_start().starts_with("/open [thread]"))
                .unwrap();
            if w < HELP_STACKED_BELOW {
                assert_eq!(rows[open].trim(), "/open [thread]", "{w}x{h}");
                assert!(rows[open + 1].starts_with("    talk"), "{w}x{h}: {rows:?}");
            } else {
                // The commands' longest entry is the /trash usage.
                let longest = crate::slash::COMMANDS
                    .iter()
                    .map(|c| c.1.chars().count())
                    .max()
                    .unwrap();
                let column = (longest + 4).min(usize::from(area.width) - HELP_TEXT_MIN);
                assert_eq!(col(&rows[open], "talk"), Some(column), "{w}x{h}: {rows:?}");
            }
        }
        // The keys and the wheel scroll it, within its rows; Esc and q close it.
        let mut st = three_threads();
        st.handle_key(key(KeyCode::Char('?')));
        draw_panes(&mut st, 80, 24);
        let max = st.view.panes.help_max;
        assert!(max > 20, "{max}");
        st.handle_key(key(KeyCode::PageDown));
        assert_eq!(st.view.help_scroll, 10);
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.view.help_scroll, 9);
        st.handle_key(key(KeyCode::End));
        assert_eq!(st.view.help_scroll, max);
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.view.help_scroll, max, "never past its end");
        st.handle_key(key(KeyCode::Home));
        assert_eq!(st.view.help_scroll, 0);
        let screen_buf = Buffer::empty(Rect::default());
        let wheel = |st: &mut AppState, kind| {
            st.handle_mouse(
                crossterm::event::MouseEvent {
                    kind,
                    column: 40,
                    row: 12,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &screen_buf,
            );
        };
        wheel(&mut st, crossterm::event::MouseEventKind::ScrollDown);
        assert_eq!(usize::from(st.view.help_scroll), crate::app::WHEEL_STEP);
        wheel(&mut st, crossterm::event::MouseEventKind::ScrollUp);
        assert_eq!(st.view.help_scroll, 0);
        // At its end on a small screen, then a bigger one (less to scroll): the wheel moves it
        // from where it shows.
        draw_panes(&mut st, 30, 20);
        st.handle_key(key(KeyCode::End));
        draw_panes(&mut st, 110, 40);
        let max = st.view.panes.help_max;
        assert!(max > 0 && st.view.help_scroll > max);
        wheel(&mut st, crossterm::event::MouseEventKind::ScrollUp);
        assert_eq!(
            usize::from(st.view.help_scroll),
            usize::from(max).saturating_sub(crate::app::WHEEL_STEP)
        );
        assert!(st.view.help);
        st.handle_key(key(KeyCode::Esc));
        assert!(!st.view.help);
        assert!(st.view.input.is_empty(), "Esc only closed the help");
        st.handle_key(key(KeyCode::Char('?')));
        st.handle_key(key(KeyCode::End));
        st.handle_key(key(KeyCode::Char('q')));
        assert!(!st.view.help, "q closes it");
        assert!(st.view.input.is_empty(), "and types nothing");
        // Opened again, it is at its top.
        st.handle_key(key(KeyCode::Char('?')));
        assert_eq!(st.view.help_scroll, 0);
    }

    /// `orch`: claude-code orchestrator, asked for `opus`, reports
    /// `claude-opus-4-1-20250805`; `api`: codex, no model asked, reports `gpt-5-codex`; `web`:
    /// claude-code asked for `sonnet`, not started; `sh`: a pty thread, nothing asked or
    /// reported; `ops`: a claude-code worker like `orch`. `ops` is selected.
    fn model_threads() -> AppState {
        use crate::app::tests::control;
        let mut st = AppState::new();
        let mut evs = Vec::new();
        // (name, runtime, requested model, reported (ext section, model))
        type Spec<'a> = (
            &'a str,
            &'a str,
            Option<&'a str>,
            Option<(&'a str, &'a str)>,
        );
        let specs: [Spec; 5] = [
            (
                "orch",
                "claude-code",
                Some("opus"),
                Some(("claude", "claude-opus-4-1-20250805")),
            ),
            ("api", "codex", None, Some(("codex", "gpt-5-codex"))),
            ("web", "claude-code", Some("sonnet"), None),
            ("sh", "pty", None, None),
            (
                "ops",
                "claude-code",
                Some("opus"),
                Some(("claude", "claude-opus-4-1-20250805")),
            ),
        ];
        for (i, (name, runtime, model, reported)) in specs.into_iter().enumerate() {
            let mut s = ThreadScript::new(format!("t-{name}"));
            s.at(10 * i as i64);
            let mut created = s.created(name);
            if let EventBody::ThreadCreated(p) = &mut created.body {
                p.runtime = runtime.into();
                let mut twr = serde_json::Map::new();
                if name == "orch" {
                    twr.insert("role".into(), "orchestrator".into());
                }
                if let Some(m) = model {
                    twr.insert("model".into(), m.into());
                }
                p.ext = serde_json::json!({ "twr": twr }).as_object().cloned();
            }
            evs.push(created);
            if let Some((section, m)) = reported {
                let mut started = s.started("s");
                if let EventBody::ThreadStarted(p) = &mut started.body {
                    p.ext = serde_json::json!({ section: { "model": m } })
                        .as_object()
                        .cloned();
                }
                evs.push(started);
            }
        }
        control(&mut st, &evs);
        st.set_focus(Some("t-ops".into()));
        st
    }

    #[test]
    fn model_ids_are_shortened_but_recognizable() {
        for (id, short) in [
            ("claude-sonnet-4-5-20250929", "sonnet-4-5"),
            ("claude-opus-4-1-20250805", "opus-4-1"),
            ("claude-3-5-haiku-20241022", "claude-3-5-haiku"),
            ("gpt-5-codex", "gpt-5-codex"),
            ("gpt-4o-2024-08-06", "gpt-4o"),
            ("o5", "o5"),
            ("grok-code-fast-1", "grok-code-fast-1"),
            ("claude-", "claude-"),
            ("20250929", "20250929"),
            ("opus", "opus"),
        ] {
            assert_eq!(short_model(id), short, "{id}");
        }
    }

    /// Cards and tiles say `runtime · model` (the reported model, else the requested one,
    /// else `default model`), on the status line when it fits and on a line of its own when
    /// not; the transcript header names the full id; the orchestrator line names the current
    /// orchestrator's runtime and model.
    #[test]
    fn cards_tiles_and_the_main_pane_say_runtime_and_model() {
        let mut st = model_threads();
        // Wide: the cards in a column, one header line each; no orchestrator card.
        let lines = draw_panes(&mut st, 160, 40);
        for (id, want) in [
            ("t-ops", "idle · claude-code · opus-4-1"),
            ("t-api", "idle · codex · gpt-5-codex"),
            ("t-web", "idle · claude-code · sonnet"),
            ("t-sh", "idle · pty · default model"),
        ] {
            let c = pane_text(&st, &lines, &card(id));
            assert!(c.contains(want), "{want:?} missing:\n{c}");
        }
        assert!(st.view.panes.get(&card("t-orch")).is_none());
        // The main pane is the orchestrator's: its full model id.
        let s = screen(&lines);
        assert!(
            s.contains("ORCHESTRATOR  claude-code · claude-opus-4-1-20250805"),
            "{s}"
        );
        // A dive names the thread, its runtime, model and mode, and how to get back.
        st.dive("t-web");
        let s = screen(&draw(&st, 160, 30));
        assert!(
            s.contains(
                "talking to web  claude-code · sonnet · mode manual · Esc returns to the \
                 orchestrator"
            ),
            "{s}"
        );
        assert!(
            s.contains("Esc back to the orchestrator · Tab/Shift-↑↓ select"),
            "{s}"
        );
        st.dive("t-sh");
        let s = screen(&draw(&st, 160, 30));
        assert!(
            s.contains("talking to sh  pty · default model · mode manual"),
            "{s}"
        );
        st.back();
        st.set_focus(Some("t-ops".into()));

        // Narrow: one 30-column card; the runtime and model get their own line.
        let lines = draw(&st, 30, 24);
        assert!(
            lines[0].contains("┏ proj · ops ") && lines[0].contains(" manual "),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1].trim_matches(['┃', ' ']),
            "idle",
            "{}",
            screen(&lines)
        );
        assert_eq!(
            lines[2].trim_matches(['┃', ' ']),
            "claude-code · opus-4-1",
            "{}",
            screen(&lines)
        );
        assert!(lines[3].contains("› not started"), "{}", screen(&lines));
        assert!(!lines[4].contains("proj"), "{}", screen(&lines));

        // Grid: tiles carry the box too, and fall back to two lines when narrow.
        st.view.screen = Screen::Grid;
        let lines = draw(&st, 180, 30);
        let s = screen(&lines);
        assert!(s.contains("idle · claude-code@local · opus-4-1"), "{s}");
        assert!(s.contains("idle · pty@local · default model"), "{s}");
        assert!(s.contains("grid · 4 threads"), "no orchestrator tile: {s}");
        let lines = draw(&st, 32, 30);
        let at = lines
            .iter()
            .position(|l| l.starts_with("┏ proj · ops "))
            .unwrap();
        let body = |i: usize| lines[at + i].trim_matches(['┃', ' ']).to_string();
        assert_eq!(
            [body(1), body(2)],
            ["idle", "claude-code@local · opus-4-1"],
            "{}",
            screen(&lines)
        );

        // `/orch` alone: the picker marks the current orchestrator's runtime.
        st.view.screen = Screen::Stack;
        for c in "/orch".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        st.handle_key(key(KeyCode::Enter));
        let lines = draw(&st, 200, 24);
        let s = screen(&lines);
        assert!(s.contains("switch the orchestrator"), "{s}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("› 1 claude-code") && l.contains("(current)")),
            "{s}"
        );
    }

    /// A thread's reasoning effort follows its model on its card, its tile and the dive
    /// header; a `thread.model_changed` shows there at once, and a cleared effort leaves the
    /// model alone.
    #[test]
    fn cards_tiles_and_the_dive_header_show_the_effort() {
        use crate::app::tests::control;
        use twr_core::event::{ChangedBy, ThreadModelChanged};
        let mut st = AppState::new();
        let mut s = ThreadScript::new("t-web");
        let mut created = s.created("web");
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.runtime = "claude-code".into();
            p.ext = serde_json::json!({"twr": {"model": "sonnet", "effort": "high"}})
                .as_object()
                .cloned();
        }
        control(&mut st, &[created]);
        st.set_focus(Some("t-web".into()));
        let lines = draw_panes(&mut st, 160, 40);
        let c = pane_text(&st, &lines, &card("t-web"));
        assert!(c.contains("idle · claude-code · sonnet · high"), "{c}");
        st.view.screen = Screen::Grid;
        let g = screen(&draw(&st, 180, 30));
        assert!(
            g.contains("idle · claude-code@local · sonnet · high"),
            "{g}"
        );
        st.view.screen = Screen::Stack;
        st.dive("t-web");
        let d = screen(&draw(&st, 160, 30));
        assert!(
            d.contains("talking to web  claude-code · sonnet · high · mode manual"),
            "{d}"
        );
        let changed = s.event(EventBody::ThreadModelChanged(ThreadModelChanged {
            model: Some("claude-fable-5-1".into()),
            effort: None,
            previous_model: Some("sonnet".into()),
            previous_effort: Some("high".into()),
            by: ChangedBy::User,
            ..Default::default()
        }));
        control(&mut st, &[changed]);
        let d = screen(&draw(&st, 160, 30));
        assert!(
            d.contains("talking to web  claude-code · fable-5-1 · mode manual"),
            "{d}"
        );
    }

    /// `/orch` alone lists the runtimes that can host the orchestrator: the built-in ones
    /// until the hub's list arrives, then the hub's.
    #[test]
    fn the_orchestrator_picker_lists_the_hubs_runtimes() {
        let mut st = three_threads();
        let open = |st: &mut AppState| {
            st.view.input.clear();
            for c in "/orch".chars() {
                st.handle_key(key(KeyCode::Char(c)));
            }
            st.handle_key(key(KeyCode::Enter));
            screen(&draw(st, 160, 24))
        };
        let s = open(&mut st);
        for r in ["1 claude-code", "2 codex", "3 grok-build"] {
            assert!(s.contains(r), "{r}: {s}");
        }
        assert!(s.contains("Esc cancel"), "{s}");
        st.handle_key(key(KeyCode::Esc));
        st.view.orch_runtimes = vec!["codex".into(), "gemini".into()];
        let s = open(&mut st);
        assert!(s.contains("1 codex") && s.contains("2 gemini"), "{s}");
        assert!(!s.contains("grok-build"), "{s}");
    }

    #[test]
    fn the_first_run_picker_is_drawn_over_the_view() {
        let mut st = three_threads();
        st.view.picker = crate::app::Picker::from_show(&serde_json::json!({
            "thread": null, "retired": 0,
            "configured": {"runtime": "codex", "explicit": false},
            "runtimes": ["claude-code", "codex", "gemini"],
            "installed": {"claude-code": true, "codex": false},
        }));
        let lines = draw(&st, 120, 30);
        let s = screen(&lines);
        for part in [
            "choose the orchestrator",
            "Pick the agent that runs your orchestrator",
            "(you can switch any time with /orch)",
            "1 claude-code  installed",
            "› 2 codex        not installed  (default)",
            "Enter choose · Esc decide later",
        ] {
            assert!(s.contains(part), "{part:?} missing:\n{s}");
        }
        // Not known to the hub: no mark.
        let gemini = lines.iter().find(|l| l.contains("3 gemini")).unwrap();
        assert!(!gemini.contains("installed"), "{gemini}");
        // Gone once chosen.
        st.view.picker = None;
        assert!(!screen(&draw(&st, 120, 30)).contains("choose the orchestrator"));
    }

    #[test]
    fn the_help_lists_the_typing_keys_and_the_mouse() {
        let s = screen(
            &help_lines(1000)
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>(),
        );
        for part in [
            "Keys (typing)",
            "Ctrl-←→, Alt-←→, Alt-B/F",
            "Ctrl-W",
            "Ctrl-K",
            "Home, End",
            "Shift-Enter, Alt-Enter, Ctrl-J",
            "/mouse [on|off]",
            "Shift-drag",
        ] {
            assert!(s.contains(part), "{part:?} missing:\n{s}");
        }
    }

    fn decisions_state() -> AppState {
        let mut st = with_decisions();
        open_decisions(&mut st);
        st
    }

    /// Opens the decisions overlay: `/decisions` typed, then Enter.
    fn open_decisions(st: &mut AppState) {
        for c in "/decisions".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.screen, Screen::Decisions);
    }

    /// The table body rows (below the border and the header) of a decisions screen.
    fn table_rows(lines: &[String]) -> Vec<String> {
        lines[4..]
            .iter()
            .take_while(|l| l.starts_with('│'))
            .filter(|l| !l.trim_matches(|c| c == '│' || c == ' ').is_empty())
            .cloned()
            .collect()
    }

    #[test]
    fn decision_table_shows_every_column_newest_first() {
        let st = decisions_state();
        let lines = draw(&st, 160, 20);
        assert!(
            lines[0].contains(
                "decisions · 6 · code 4 · orchestrator 1 · user 1 · \
                 route fell through to orchestrator/user: 2/3 (67%)"
            ),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("site all (f) · provider all (v) · 6 shown"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("decision.made · newest first"),
            "{}",
            lines[2]
        );
        let header = &lines[3];
        for h in [
            "time", "thread", "turn", "site", "provider", "chosen", "conf", "latency", "options",
        ] {
            assert!(header.contains(h), "{h}: {header}");
        }
        let rows = table_rows(&lines);
        assert_eq!(rows.len(), 6, "{}", screen(&lines));
        // Cells by column position: fixed widths, one space apart, options take the rest.
        let words = |r: &str| -> Vec<String> {
            let chars: Vec<char> = r.trim_matches('│').chars().collect();
            let mut out = Vec::new();
            let mut x = 0;
            for (_, w) in DECISION_COLUMNS {
                let w = usize::from(w);
                out.push(
                    chars[x..x + w]
                        .iter()
                        .collect::<String>()
                        .trim()
                        .to_string(),
                );
                x += w + 1;
            }
            out.push(chars[x..].iter().collect::<String>().trim().to_string());
            out
        };
        assert_eq!(
            words(&rows[0]),
            [
                "01:03:21",
                "alpha",
                "2",
                "block_kind",
                "code",
                "user_blocked",
                "1.00",
                "3ms",
                "transient, user_blocked"
            ],
            "{}",
            rows[0]
        );
        assert_eq!(
            words(&rows[1])[..8],
            [
                "01:03:20",
                "alpha",
                "1",
                "block_kind",
                "code",
                "transient",
                "1.00",
                "0ms"
            ]
        );
        assert_eq!(
            words(&rows[2])[..8],
            ["01:01:40", "beta", "1", "grade", "code", "success", "0.60", "0ms"]
        );
        // Hub route decisions: thread "hub", no turn.
        assert_eq!(
            words(&rows[4])[..8],
            [
                "01:00:01",
                "hub",
                "—",
                "route",
                "orchestrator",
                "orchestrator",
                "1.00",
                "1ms"
            ]
        );
        assert_eq!(
            words(&rows[5])[..8],
            ["01:00:00", "hub", "—", "route", "code", "alpha", "1.00", "0ms"]
        );
        assert_eq!(
            words(&rows[3])[..7],
            ["01:00:02", "hub", "—", "route", "user", "gamma", "1.00"]
        );
        // Long option lists are cut to the column with an ellipsis.
        let lines = draw(&st, 100, 20);
        let top = &table_rows(&lines)[5];
        assert!(top.ends_with("alpha, beta, ga…│"), "{top}");
        assert!(!top.contains("new thread"));
    }

    #[test]
    fn the_selected_decision_is_reversed() {
        let st = decisions_state();
        let buf = draw_buf(&st, 160, 20);
        let row_y = |n: u16| 4 + n;
        // Row 0 is selected.
        assert!(buf[(2, row_y(0))].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(2, row_y(1))].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn decision_filters_show_in_the_table() {
        let mut st = decisions_state();
        st.handle_key(key(KeyCode::Char('f')));
        st.handle_key(key(KeyCode::Char('v')));
        st.handle_key(key(KeyCode::Char('v')));
        let lines = draw(&st, 160, 20);
        assert!(
            lines[1].contains("site route (f) · provider orchestrator (v) · 1 shown"),
            "{}",
            lines[1]
        );
        let rows = table_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("orchestrator") && rows[0].contains("hub"));
        // The summary counts every kept decision, whatever the filters.
        assert!(lines[0].contains("decisions · 6 · code 4"), "{}", lines[0]);
        // Nothing matches: the table says so.
        st.handle_key(key(KeyCode::Char('f')));
        st.handle_key(key(KeyCode::Char('f')));
        assert_eq!(
            (
                st.view.decision_filter.site,
                st.view.decision_filter.provider
            ),
            (
                Some(twr_core::event::DecisionSite::BlockKind),
                Some(DecisionProvider::Orchestrator)
            )
        );
        let s = screen(&draw(&st, 160, 20));
        assert!(s.contains("no decisions match the filters"), "{s}");
        // No decisions at all.
        let mut empty = five_threads();
        open_decisions(&mut empty);
        let s = screen(&draw(&empty, 160, 20));
        assert!(s.contains("no decisions yet"), "{s}");
        assert!(
            s.contains("route fell through to orchestrator/user: 0/0 (—)"),
            "{s}"
        );
    }

    #[test]
    fn decision_table_scrolls_to_the_selection_and_reports_the_cap() {
        use crate::app::tests::decision;
        use crate::app::DECISIONS_CAP;
        use twr_core::event::DecisionSite;
        use twr_core::proto::{StreamItem, StreamKind};
        let mut st = five_threads();
        let mut h = ThreadScript::new(HUB_THREAD);
        for i in 0..DECISIONS_CAP + 5 {
            let ev = decision(
                &mut h,
                None,
                DecisionSite::Route,
                DecisionProvider::Code,
                &["x"],
                &format!("d{i}"),
                1.0,
                0,
            );
            st.apply_event(&StreamItem {
                stream: StreamKind::Control,
                gseq: Some(st.last_gseq + 1),
                event: ev,
            });
        }
        open_decisions(&mut st);
        let lines = draw(&st, 160, 17);
        assert!(
            lines[0].contains(&format!(
                "decisions · {DECISIONS_CAP} kept of {} (last {DECISIONS_CAP})",
                DECISIONS_CAP + 5
            )),
            "{}",
            lines[0]
        );
        // 17 rows: summary, filters, 14 table rows (border, header, 11 body, border), status
        // (the overlay has no input box).
        let rows = table_rows(&lines);
        assert_eq!(rows.len(), 11);
        assert!(rows[0].contains("d2004"), "{}", rows[0]);
        let s = screen(&lines);
        assert!(
            s.contains(&format!("{} older ↓", DECISIONS_CAP - 11)),
            "{s}"
        );
        assert!(!s.contains("newer"));
        // Paging down keeps the selected row (index 20) on screen, at the bottom.
        st.handle_key(key(KeyCode::PageDown));
        st.handle_key(key(KeyCode::PageDown));
        let lines = draw(&st, 160, 17);
        let rows = table_rows(&lines);
        assert!(rows[10].contains("d1984"), "{}", screen(&lines));
        assert!(screen(&lines).contains("↑ 10 newer"));
        assert_eq!(table_first_row(10, 8), 3);
        assert_eq!(table_first_row(2, 8), 0);
    }

    #[test]
    fn decisions_key_help() {
        let mut st = decisions_state();
        let last = draw(&st, 160, 20).pop().unwrap();
        assert!(
            last.contains("Enter open thread")
                && last.contains("f site")
                && last.contains("v provider")
                && last.contains("Esc close")
                && last.contains("? help"),
            "{last}"
        );
        st.handle_key(key(KeyCode::Esc));
        let last = draw(&st, 180, 20).pop().unwrap();
        assert!(last.contains("Ctrl-G grid"), "{last}");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate("abc", 0), "");
    }

    /// The main pane is the orchestrator conversation: its header names the orchestrator's
    /// runtime and full model; the user's messages and its replies show, its tool calls and
    /// the hub's notices fold to one dim line each. At any width.
    #[test]
    fn the_main_pane_is_the_orchestrator_conversation() {
        use crate::app::tests::{orch_conversation, with_orch};
        let mut st = crate::app::tests::without_worker_requests(with_orch());
        orch_conversation(&mut st);
        let lines = draw_panes(&mut st, 160, 40);
        let s = screen(&lines);
        assert!(
            lines[0].starts_with(" ORCHESTRATOR  claude-code · claude-opus-4-1-20250805 "),
            "{s}"
        );
        for part in [
            "❯ which threads need me?",
            "  Listed the threads, used 1 twr tool (ctrl+o to expand)",
            "● beta waits for a permission to run rm -rf target; alpha gave up.",
            "  hub update (ctrl+o to expand)",
            "● gamma finished its tests: all green.",
        ] {
            assert!(s.contains(part), "{part:?} missing:\n{s}");
        }
        assert!(
            !s.contains("mcp__twr__") && !s.contains("[twr notice]"),
            "{s}"
        );
        // The folded lines are dim, the prompt on a grey band.
        let buf = crate::app::tests::draw(&mut st, 160, 40);
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        assert_eq!(buf[(a.x + 2, a.y + 2)].symbol(), "L");
        assert_eq!(buf[(a.x + 2, a.y + 2)].fg, MUTED);
        assert_eq!(buf[(a.x + 4, a.y)].bg, Color::Indexed(237));
        // Narrow: cards in a row above, the header cut to fit.
        let lines = draw_panes(&mut st, 60, 30);
        let head = lines
            .iter()
            .find(|l| l.starts_with(" ORCHESTRATOR"))
            .unwrap_or_else(|| panic!("{}", screen(&lines)));
        assert!(
            head.contains("ORCHESTRATOR  claude-code · claude-opus-4-1-20250805"),
            "{head}"
        );
        let lines = draw_panes(&mut st, 40, 30);
        let head = lines
            .iter()
            .find(|l| l.starts_with(" ORCHESTRATOR"))
            .unwrap();
        assert_eq!(
            head,
            &format!("{:40}", " ORCHESTRATOR  claude-code · claude-op…")
        );
        // Very narrow: the label alone, then the label cut.
        let lines = draw_panes(&mut st, 20, 30);
        assert!(lines
            .iter()
            .any(|l| l == &format!("{:20}", " ORCHESTRATOR")));
        let lines = draw_panes(&mut st, 8, 30);
        assert!(lines.iter().any(|l| l == " ORCHE… "), "{}", screen(&lines));
        // Before an orchestrator exists: the one the hub would start.
        let mut st = three_threads();
        st.view.orch_default = Some(("codex".into(), None));
        let s = screen(&draw(&st, 160, 30));
        assert!(
            s.contains("ORCHESTRATOR  codex · default model · starts with your first message"),
            "{s}"
        );
    }

    /// The orchestrator's open items show on top of its conversation, selectable like a card.
    #[test]
    fn the_orchestrators_items_show_on_top_of_its_conversation() {
        use crate::app::tests::{control, orch_conversation, with_orch};
        let mut st = with_orch();
        orch_conversation(&mut st);
        let mut o = ThreadScript::new("t-orch");
        o.at(300);
        let mut err = o.raise("oe1", AttentionReason::Error);
        if let EventBody::AttentionRaised(p) = &mut err.body {
            p.summary = "orchestrator crashed (exit 1)".into();
            p.options = vec!["retry".into(), "dismiss".into()];
        }
        control(&mut st, &[err]);
        let lines = draw_panes(&mut st, 160, 40);
        let banner = pane_text(&st, &lines, &card("t-orch"));
        assert!(banner.contains("the orchestrator needs you"), "{banner}");
        assert!(
            banner.contains(" ERROR  orchestrator crashed (exit 1)"),
            "{banner}"
        );
        assert!(banner.contains("1 retry  2 dismiss"), "{banner}");
        assert!(banner.starts_with('┌'), "not selected: {banner}");
        st.set_focus(Some("t-orch".into()));
        let lines = draw_panes(&mut st, 160, 40);
        assert!(pane_text(&st, &lines, &card("t-orch")).starts_with('┏'));
        // Never a card.
        let s = screen(&lines);
        assert_eq!(s.matches(" ERROR ").count(), 1, "{s}");
        // The grid says so in its header.
        st.view.screen = Screen::Grid;
        let lines = draw(&st, 160, 30);
        assert!(
            lines[0].contains("the orchestrator needs you"),
            "{}",
            lines[0]
        );
    }

    /// What the renderer reports as drawn in full (badge, summary and every option): the only
    /// items an option key may answer. The orchestrator's banner only in the main
    /// conversation; cards only where they are drawn; nothing cut short or under an overlay.
    #[test]
    fn only_items_drawn_in_full_with_their_options_are_reported() {
        use crate::app::tests::{control, with_orch};
        let drawn = |st: &mut AppState, w: u16, h: u16| -> Vec<(String, String)> {
            draw_panes(st, w, h);
            st.view
                .panes
                .items
                .iter()
                .map(|i| (i.thread.clone(), i.attn_id.clone()))
                .collect()
        };
        let pair = |t: &str, a: &str| (t.to_string(), a.to_string());
        let mut st = with_orch();
        let mut o = ThreadScript::new("t-orch");
        o.at(300);
        let mut q = o.raise("oq1", AttentionReason::Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = "Which database?".into();
            p.options = vec!["Postgres".into(), "SQLite".into()];
        }
        control(&mut st, &[q]);
        let stack = drawn(&mut st, 160, 40);
        for want in [
            pair("t-orch", "oq1"),
            pair("t-beta", "pm1"),
            pair("t-alpha", "rv1"),
        ] {
            assert!(stack.contains(&want), "{want:?} in {stack:?}");
        }
        // Each one's revision as drawn.
        for i in &st.view.panes.items {
            assert_eq!(i.revision, st.attention.get(&i.attn_id).unwrap().revision);
        }
        // A dive hides the banner; the cards stay.
        st.dive("t-gamma");
        let dive = drawn(&mut st, 160, 40);
        assert!(!dive.iter().any(|(t, _)| t == "t-orch"), "{dive:?}");
        assert!(dive.contains(&pair("t-beta", "pm1")), "{dive:?}");
        st.back();
        // The grid draws tiles with their options, never the banner.
        st.view.screen = Screen::Grid;
        let grid = drawn(&mut st, 180, 30);
        assert!(!grid.iter().any(|(t, _)| t == "t-orch"), "{grid:?}");
        assert!(grid.contains(&pair("t-beta", "pm1")), "{grid:?}");
        st.view.screen = Screen::Stack;
        // The help overlay hides everything under it.
        st.view.help = true;
        assert!(drawn(&mut st, 160, 40).is_empty());
        st.view.help = false;
        // A narrow row of cards: only the cards in it.
        st.set_focus(Some("t-gamma".into()));
        let narrow = drawn(&mut st, 60, 24);
        assert!(!narrow.iter().any(|(t, _)| t == "t-beta"), "{narrow:?}");
        assert!(narrow.contains(&pair("t-alpha", "rv1")), "{narrow:?}");
        // A summary too long for any card: its options never show whole, so it is never
        // reported (nor answerable).
        let mut b = ThreadScript::new("t-beta");
        b.at(400);
        let mut long = b.raise("pm1", AttentionReason::Permission);
        if let EventBody::AttentionRaised(p) = &mut long.body {
            p.summary = "rm ".repeat(4000);
        }
        control(&mut st, &[long]);
        st.set_focus(Some("t-beta".into()));
        let cut = drawn(&mut st, 160, 40);
        assert!(!cut.iter().any(|(t, _)| t == "t-beta"), "{cut:?}");
    }

    /// Option rows: options go whole on a row, the next row when one does not fit; rows never
    /// exceed the width, so drawing them shows every option in full.
    #[test]
    fn option_rows_keep_each_option_whole() {
        let text = |rows: &[Line]| -> Vec<String> {
            rows.iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let opts: Vec<String> = ["allow", "deny", "allow always"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            text(&option_rows(&opts, 40)),
            ["1 allow  2 deny  3 allow always"]
        );
        assert_eq!(
            text(&option_rows(&opts, 20)),
            ["1 allow  2 deny", "3 allow always"]
        );
        let long = vec!["x".repeat(25)];
        let rows = option_rows(&long, 10);
        assert!(rows.iter().all(|l| l.width() <= 10), "{:?}", text(&rows));
        assert_eq!(
            text(&rows).concat(),
            format!("1 {}", "x".repeat(25)).replace(' ', "")
        );
        // Every option: the tenth on is numbered `10.` (no key answers it) but drawn.
        let many: Vec<String> = (1..=11).map(|n| format!("o{n}")).collect();
        let all = text(&option_rows(&many, 200)).concat();
        assert!(all.starts_with("1 o1  2 o2"), "{all}");
        assert!(all.ends_with("9 o9  10. o10  11. o11"), "{all}");
        let rows = option_rows(&many, 200);
        let ten = rows[0].spans.iter().find(|s| s.content == "10.").unwrap();
        assert_ne!(ten.style.fg, Some(Color::Yellow), "no key");
        // Wrapping breaks at spaces and keeps the styles.
        let line = Line::from(vec![
            Span::styled("ab", Style::default().fg(Color::Red)),
            Span::raw(" cd ef"),
        ]);
        let rows = wrap_line(line, 5);
        assert_eq!(text(&rows), ["ab cd", "ef"]);
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
        // A hard line break starts a row (a `\r` is dropped), even when the whole fits on one;
        // each part wraps on its own and keeps its styles.
        let line = Line::from(vec![
            Span::styled("ab\r\ncd", Style::default().fg(Color::Red)),
            Span::raw(" ef\n\ngh ij kl"),
        ]);
        let rows = wrap_line(line.clone(), 40);
        assert_eq!(text(&rows), ["ab", "cd ef", "", "gh ij kl"]);
        assert_eq!(rows[1].spans[0].style.fg, Some(Color::Red));
        assert_eq!(rows[1].spans[1].style.fg, None);
        assert_eq!(
            text(&wrap_line(line, 5)),
            ["ab", "cd ef", "", "gh ij", "kl"]
        );
        // An option holding a line break gets rows of its own.
        let opts = vec![
            "yes".to_string(),
            "no,\nnot now".to_string(),
            "later".to_string(),
        ];
        assert_eq!(
            text(&option_rows(&opts, 40)),
            ["1 yes", "2 no,", "not now", "3 later"]
        );
        // A card keeps the item whole first; the body gives way.
        let l = |s: &str| Line::from(s.to_string());
        let (rows, whole) = fit_rows(
            vec![l("status"), l("project")],
            &[l("Q"), l("1 a")],
            &[l("note")],
            3,
        );
        assert!(whole);
        assert_eq!(text(&rows), ["Q", "1 a", "note"]);
        let (_, whole) = fit_rows(vec![], &[l("Q"), l("1 a")], &[], 1);
        assert!(!whole);
    }

    /// A card or tile of a thread at work with nothing open says what it is doing now
    /// (`thread.activity`'s `activity`) in its slot, on one row, cut to the width with `…`
    /// rather than wrapped; before the hub said, what its status says. A card with an item
    /// open shows the item there instead, not the activity too. So at every width: beside
    /// the conversation, in the row above it and in the grid.
    #[test]
    fn cards_and_tiles_say_what_each_thread_is_doing() {
        let mut st = three_threads();
        let long = format!(
            "implementing scroll anchoring · running tests {}",
            "x".repeat(200)
        );
        st.apply_result(
            "thread.activity",
            &serde_json::json!({
                "t-alpha": {"activity": "thinking", "doing": "thinking"},
                "t-gamma": {"activity": long, "doing": "Bash: cargo test", "said": "all green"},
            }),
        );
        assert!(!st.activity_pending);
        for w in [160, 99] {
            let mut st2 = st.clone();
            let lines = draw_panes(&mut st2, w, 40);
            let gamma = pane_text(&st2, &lines, &card("t-gamma"));
            assert!(gamma.contains("› implementing scroll"), "{w}: {gamma}");
            let row = gamma.lines().find(|l| l.contains('›')).unwrap();
            assert!(row.contains('…'), "cut with …: {row}");
            assert!(
                gamma.lines().filter(|l| l.contains("xxx")).count() <= 1,
                "one row: {gamma}"
            );
            // alpha's review and beta's permission take the slot.
            let alpha = pane_text(&st2, &lines, &card("t-alpha"));
            assert!(
                alpha.contains(" REVIEW ") && !alpha.contains('›'),
                "{w}: {alpha}"
            );
            let beta = pane_text(&st2, &lines, &card("t-beta"));
            assert!(
                beta.contains(" PERMISSION ") && !beta.contains('›'),
                "{w}: {beta}"
            );
        }
        // Before the hub said what gamma does: its status.
        let mut quiet = three_threads();
        let lines = draw_panes(&mut quiet, 160, 40);
        let gamma = pane_text(&quiet, &lines, &card("t-gamma"));
        assert!(gamma.contains("› working"), "{gamma}");
        st.view.screen = Screen::Grid;
        let s = screen(&draw(&st, 160, 40));
        assert!(
            s.contains("› implementing scroll anchoring")
                && !s.contains("› thinking")
                && !s.contains("› waiting for you"),
            "{s}"
        );
        // A failed poll keeps what was known and lets the next one go.
        st.activity_pending = true;
        st.apply_result("thread.activity", &serde_json::Value::Null);
        assert!(!st.activity_pending);
        assert_eq!(st.activity["t-alpha"].activity, "thinking");
    }

    /// The selected card is drawn selected (thick, yellow) while typing goes on, told apart
    /// from where the line goes: the main pane's thick border in the orchestrator's color, and
    /// in a dive the thread's card, thin, in the dive's color. Nothing selected, no card is
    /// drawn so; selected, the dive's card is drawn selected.
    #[test]
    fn the_selection_is_drawn_apart_from_where_the_line_goes() {
        let mut st = three_threads();
        st.set_focus(Some("t-gamma".into()));
        let lines = draw(&st, 160, 40);
        let s = screen(&lines);
        assert!(s.contains("┏ proj · gamma "), "{s}");
        assert!(lines[1].starts_with('┃'), "where the line goes: {s}");
        st.set_focus(None);
        let s = screen(&draw(&st, 160, 40));
        assert!(!s.contains('┏'), "nothing selected: {s}");
        st.view.screen = Screen::Grid;
        st.set_focus(Some("t-gamma".into()));
        let s = screen(&draw(&st, 160, 40));
        assert!(s.contains("┏ proj · gamma "), "a tile too: {s}");
        st.view.screen = Screen::Stack;
        st.dive("t-beta");
        let mut st2 = st.clone();
        let buf = crate::app::tests::draw(&mut st2, 160, 40);
        let lines = buffer_lines(&buf);
        let s = screen(&lines);
        assert!(!s.contains('┏'), "the dive selects nothing: {s}");
        let row = lines
            .iter()
            .position(|l| l.contains("┌ proj · beta "))
            .unwrap();
        let x = col(&lines[row], "┌ proj · beta ").unwrap() as u16;
        assert_eq!(buf[(x, row as u16)].fg, target_color(true), "{s}");
        assert_eq!(buf[(0, 1)].fg, target_color(true), "the main pane: {s}");
        st.set_focus(Some("t-beta".into()));
        let s = screen(&draw(&st, 160, 40));
        assert!(
            s.contains("┏ proj · beta "),
            "selected, the dive's card: {s}"
        );
    }

    /// A worker's request in the conversation shows what it was raised in: why the runtime
    /// asks, where, the turn's task and what the agent last said. Its card says why.
    #[test]
    fn a_request_shows_why_where_and_what_the_agent_said() {
        use crate::app::tests::control;
        let mut st = three_threads();
        let mut d = ThreadScript::new("t-delta");
        d.at(30);
        let mut perm = d.raise("pm9", AttentionReason::Permission);
        if let EventBody::AttentionRaised(p) = &mut perm.body {
            p.summary = "Allow shell: git commit -m fix".into();
            p.options = vec!["allow".into(), "deny".into()];
            p.ext = serde_json::json!({
                "codex": {"reason": "Git metadata is outside the workspace", "cwd": "/w/delta"},
                "twr": {"context": {"said": "Tests pass; committing now.", "task": "fix the bug"}},
            })
            .as_object()
            .cloned();
        }
        control(
            &mut st,
            &[d.created("delta"), d.turn_started("fix the bug"), perm],
        );
        let item = st.attention.get("pm9").unwrap();
        let c = item.context.as_ref().unwrap();
        assert_eq!(c.cwd.as_deref(), Some("/w/delta"));
        let s = flat(&draw(&st, 160, 60));
        for part in [
            "why: Git metadata is outside the workspace",
            "in: /w/delta",
            "task: fix the bug",
            "said: Tests pass; committing now.",
        ] {
            assert!(s.contains(part), "{part:?} missing: {s}");
        }
        // The card has room for the why only.
        let mut st2 = st.clone();
        st2.set_focus(Some("t-delta".into()));
        let lines = draw_panes(&mut st2, 160, 60);
        let delta = pane_text(&st2, &lines, &card("t-delta"));
        assert!(delta.contains("why: Git metadata"), "{delta}");
        assert!(!delta.contains("said:"), "{delta}");
        // A long reason takes one row of the card: its status stays.
        let mut long = d.raise("pm9", AttentionReason::Permission);
        if let EventBody::AttentionRaised(p) = &mut long.body {
            p.summary = "Allow shell: git commit -m fix".into();
            p.options = vec!["allow".into(), "deny".into()];
            p.ext = serde_json::json!({"codex": {"reason": "because ".repeat(100)}})
                .as_object()
                .cloned();
        }
        control(&mut st2, &[long]);
        let lines = draw_panes(&mut st2, 160, 60);
        let delta = pane_text(&st2, &lines, &card("t-delta"));
        assert_eq!(
            delta.lines().filter(|l| l.contains("because")).count(),
            1,
            "{delta}"
        );
        assert!(delta.contains("waiting ·"), "{delta}");
    }

    /// A question waiting for `y` too long for one row wraps in the status bar: all of it is
    /// on screen.
    #[test]
    fn a_long_question_in_the_status_bar_wraps() {
        let mut st = three_threads();
        let items: Vec<serde_json::Value> = (0..12)
            .map(|i| {
                serde_json::json!({"item": format!("01J00000000000000000ITEM{i:02}"),
                                    "name": format!("worker-{i}")})
            })
            .collect();
        st.apply_result(
            "trash.empty",
            &serde_json::json!({"dry_run": true, "items": items}),
        );
        let prompt = st.view.confirm.as_ref().expect("asks y").prompt.clone();
        assert!(prompt.chars().count() > 80, "{prompt}");
        let lines = draw(&st, 80, 30);
        let bar = flat(&lines[20..]);
        assert!(bar.contains(&flat(&[prompt])), "{bar}");
    }

    /// The input box is always drawn, empty or not, with a card selected or not: typing
    /// always goes to it. The decisions overlay has none.
    #[test]
    fn the_input_box_is_always_drawn() {
        let mut st = three_threads();
        draw_panes(&mut st, 120, 30);
        assert_eq!(st.view.panes.get(&PaneId::Input).unwrap().area.height, 1);
        st.set_focus(Some("t-beta".into()));
        st.view.input.set("draft");
        let s = screen(&draw_panes(&mut st, 120, 30));
        assert!(st.view.panes.get(&PaneId::Input).is_some(), "{s}");
        assert!(s.contains("→ orchestrator  Enter sends"), "{s}");
        st.view.screen = Screen::Grid;
        draw_panes(&mut st, 120, 30);
        assert!(st.view.panes.get(&PaneId::Input).is_some());
        st.view.screen = Screen::Decisions;
        draw_panes(&mut st, 120, 30);
        assert!(st.view.panes.get(&PaneId::Input).is_none());
    }

    /// The hints name Shift-Enter for a new line only where it is known to arrive apart from
    /// Enter (the keyboard protocol is on, or one came in); elsewhere (mosh, a terminal
    /// without the protocol, tmux) Ctrl-J, which works everywhere.
    #[test]
    fn the_hints_name_shift_enter_only_where_it_arrives() {
        let mut st = three_threads();
        for dive in [false, true] {
            if dive {
                st.dive("t-beta");
            }
            st.view.shift_enter = false;
            let h = hints(&st);
            assert!(
                h.contains("Ctrl-J new line") && !h.contains("Shift-Enter"),
                "{h}"
            );
            st.view.shift_enter = true;
            let h = hints(&st);
            assert!(
                h.contains("Shift-Enter new line") && !h.contains("Ctrl-J"),
                "{h}"
            );
        }
        // Every view with the input box names one of them; the decisions overlay has none.
        st.view.dive = None;
        st.view.shift_enter = false;
        for screen in [Screen::Stack, Screen::Grid] {
            st.view.screen = screen;
            assert!(hints(&st).contains("Ctrl-J new line"), "{screen:?}");
        }
        st.view.screen = Screen::Decisions;
        assert!(!hints(&st).contains("new line"));
    }

    /// In a dive, the input line and its placeholder say who a line goes to.
    #[test]
    fn the_input_line_says_who_a_line_goes_to() {
        let mut st = three_threads();
        let s = screen(&draw(&st, 120, 30));
        assert!(s.contains("message the orchestrator…"), "{s}");
        st.dive("t-beta");
        let s = screen(&draw(&st, 160, 30));
        assert!(
            s.contains("┌ → beta  Enter sends · Esc: back to the orchestrator ─"),
            "{s}"
        );
        assert!(s.contains("message beta directly…"), "{s}");
        let last = draw(&st, 200, 30).pop().unwrap();
        assert!(
            last.contains(
                "Enter send to this thread · Ctrl-J new line · Esc back to the orchestrator"
            ),
            "{last}"
        );
    }

    /// The main pane's header is a full-width band filled with where a line goes, its label
    /// bold; the input box's label has the same fill, its border the same color. The
    /// orchestrator and a dive never look alike.
    #[test]
    fn the_header_band_and_the_input_box_say_where_a_line_goes() {
        use crate::app::tests::with_orch;
        let mut st = with_orch();
        let main_w = 120 - side_width(120);
        let check = |st: &mut AppState, fill: Style, border: Color, label: &str| {
            let buf = crate::app::tests::draw(st, 120, 40);
            let band: String = (0..main_w).map(|x| buf[(x, 0)].symbol()).collect();
            assert!(band.starts_with(label), "{band}");
            for x in 0..main_w {
                assert_eq!(buf[(x, 0)].bg, fill.bg.unwrap(), "the whole row: {x}");
            }
            assert_eq!(buf[(1, 0)].fg, fill.fg.unwrap());
            assert!(buf[(1, 0)].modifier.contains(Modifier::BOLD));
            let input = st.view.panes.get(&PaneId::Input).unwrap().area;
            let (lx, ly) = (input.x + 1, input.y - 1);
            assert_eq!(buf[(lx, ly)].symbol(), "→");
            assert_eq!(buf[(lx, ly)].bg, fill.bg.unwrap(), "the input's label");
            assert_eq!(buf[(input.x - 1, input.y)].fg, border, "the input's border");
            assert_eq!(buf[(0, 1)].fg, border, "the main pane's border");
        };
        check(&mut st, target_fill(false), Color::Cyan, " ORCHESTRATOR  ");
        st.dive("t-beta");
        check(
            &mut st,
            target_fill(true),
            Color::Magenta,
            " ‹ back  talking to beta  ",
        );
        assert_ne!(target_fill(false), target_fill(true));
        // A working thread's icon leads the band, on the terminal's own background.
        st.dive("t-gamma");
        let buf = crate::app::tests::draw(&mut st, 120, 40);
        assert_eq!(buf[(1, 0)].symbol(), "_");
        assert_eq!(
            (buf[(1, 0)].fg, buf[(1, 0)].bg),
            (icon::ACCENT, Color::Reset)
        );
        assert_eq!(buf[(3, 0)].symbol(), "‹");
        assert_eq!(buf[(3, 0)].bg, target_fill(true).bg.unwrap());
        // With a draft and a card selected, the input's border keeps the color; its label the
        // fill.
        st.set_focus(Some("t-beta".into()));
        st.view.input.set("draft");
        let buf = crate::app::tests::draw(&mut st, 120, 40);
        let input = st.view.panes.get(&PaneId::Input).unwrap().area;
        assert_eq!(buf[(input.x - 1, input.y)].fg, Color::Magenta);
        assert_eq!(
            buf[(input.x + 1, input.y - 1)].bg,
            target_fill(true).bg.unwrap()
        );
    }

    /// The popup lists the matching commands with their descriptions above the input line.
    #[test]
    fn the_slash_popup_is_drawn_above_the_input_line() {
        let mut st = three_threads();
        for c in "/mo".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        let lines = draw_panes(&mut st, 120, 30);
        let s = screen(&lines);
        let pop = st.view.panes.get(&PaneId::Popup).unwrap().area;
        let input = st.view.panes.get(&PaneId::Input).unwrap().area;
        assert_eq!(pop.bottom() + 2, input.y, "right above the input box: {s}");
        let row = |i: u16| lines[usize::from(pop.y + i)].clone();
        assert!(
            row(0).contains("/mode [thread] <mode>") && row(0).contains("set a permission mode"),
            "{s}"
        );
        assert!(
            row(1).contains("/model <thread> [model] [--effort e]"),
            "{s}"
        );
        assert!(row(2).contains("/mouse [on|off]"), "{s}");
        assert!(s.contains(" commands "), "{s}");
        assert!(lines
            .last()
            .unwrap()
            .contains("↑↓ pick · Tab complete · Enter run · Esc close"));
        // The selected row is highlighted.
        let buf = crate::app::tests::draw(&mut st, 120, 30);
        assert!(buf[(pop.x + 1, pop.y)]
            .modifier
            .contains(Modifier::REVERSED));
        assert!(!buf[(pop.x + 1, pop.y + 1)]
            .modifier
            .contains(Modifier::REVERSED));
        // Arguments: the threads with their status and mode.
        st.view.input.set("/open ");
        let s = screen(&draw_panes(&mut st, 120, 30));
        assert!(s.contains("waiting · fake · mode manual"), "{s}");
        assert!(s.contains(" values "), "{s}");
        // Closed: gone.
        st.handle_key(key(KeyCode::Esc));
        draw_panes(&mut st, 120, 30);
        assert!(st.view.panes.get(&PaneId::Popup).is_none());
        // An `@` mention: the threads, and the keys for it.
        st.view.input.set("ask @g");
        let lines = draw_panes(&mut st, 120, 30);
        let s = screen(&lines);
        assert!(s.contains(" threads ") && s.contains("@gamma"), "{s}");
        assert!(s.contains("running · fake · mode manual"), "{s}");
        assert!(lines
            .last()
            .unwrap()
            .contains("↑↓ pick · Tab/Enter insert · Esc close"));
    }

    #[test]
    fn wrap_splits_words_and_long_tokens() {
        assert_eq!(wrap("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap("  • x", 10), ["  • x"]);
    }

    fn plain(t: &str) -> Section {
        Section::Plain(t.into())
    }

    fn boxed(name: &str, text: &str, closed: bool) -> Section {
        Section::Thread {
            name: name.into(),
            text: text.into(),
            closed,
        }
    }

    /// A thread section is the text between a `<thread name="…">` line and a `</thread>`
    /// line, each tag alone on its line (spaces around it allowed). A tag inside a line is
    /// text; an opening tag inside a section closes it and opens the next; a section open at
    /// the end is open while the reply streams and closed once it is finished; a stray
    /// closing tag is dropped; a tag still streaming in never shows.
    #[test]
    fn thread_sections_are_tags_alone_on_their_lines() {
        let reply =
            "Two updates.\n<thread name=\"alpha\">\n\nalpha finished.\n\n</thread>\nThat's all.";
        assert_eq!(
            thread_sections(reply, false),
            [
                plain("Two updates."),
                boxed("alpha", "alpha finished.", true),
                plain("That's all."),
            ]
        );
        assert_eq!(
            thread_sections("  <thread name=\" beta \">  \nok\n  </thread>  ", false),
            [boxed("beta", "ok", true)]
        );
        // Inside a line, or with no name: text.
        for text in [
            "it said <thread name=\"alpha\"> then stopped",
            "<thread name=\"alpha\"> ok </thread>",
            "<thread name=\"\">",
            "<thread>",
        ] {
            assert_eq!(thread_sections(text, false), [plain(text)], "{text}");
        }
        // No nesting: the next opening tag closes the section.
        assert_eq!(
            thread_sections("<thread name=\"a\">\none\n<thread name=\"b\">\ntwo", true),
            [boxed("a", "one", true), boxed("b", "two", false)]
        );
        assert_eq!(
            thread_sections("<thread name=\"b\">\ntwo", false),
            [boxed("b", "two", true)]
        );
        // A closing tag outside a section is dropped.
        assert_eq!(
            thread_sections("a\n</thread>\nb", false),
            [plain("a"), plain("b")]
        );
        // A tag streaming in is held back as an empty line until it is whole.
        for part in [
            "<",
            "<thr",
            "<thread name=\"alp",
            "<thread name=\"alpha\"",
            "</thr",
        ] {
            let text = format!("intro\n{part}");
            assert_eq!(thread_sections(&text, true), [plain("intro\n")], "{part}");
            assert_eq!(thread_sections(&text, false), [plain(&text)], "{part}");
        }
        assert_eq!(
            thread_sections("<thread name=\"a\">\nx\n</thr", true),
            [boxed("a", "x", false)]
        );
    }

    /// The orchestrator conversation with a reply `text` (`open`: still streaming in).
    fn orch_reply(text: &str, open: bool) -> AppState {
        let mut st = crate::app::tests::without_worker_requests(crate::app::tests::with_orch());
        st.transcript.as_mut().unwrap().set_entries(vec![
            prompt("news?"),
            Entry::Text {
                seq: 0,
                text: text.into(),
                open,
            },
        ]);
        st
    }

    const ALPHA_REPLY: &str = "Here is the news.\n<thread name=\"alpha\">\nalpha finished the \
        refactor and every test passes on the first run.\n</thread>\n<thread name=\"nobody\">\n\
        who is that\n</thread>";

    /// A thread section is a rounded box across the pane in the thread's color, its name
    /// and status in the top border; a name that matches no thread gets a dim box with that
    /// name. The tags never show.
    #[test]
    fn a_thread_section_is_a_box_in_the_threads_color() {
        let mut st = orch_reply(ALPHA_REPLY, false);
        let buf = crate::app::tests::draw(&mut st, 120, 30);
        let lines: Vec<String> = (0..30)
            .map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let rows = transcript_rows(&st, &lines);
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let w = usize::from(a.width);
        assert_eq!(rows[2], "● Here is the news.");
        assert!(rows[3].starts_with("╭─ alpha · running ─"), "{rows:#?}");
        assert!(rows[3].ends_with("─╮"), "{rows:#?}");
        assert_eq!(rows[3].chars().count(), w);
        assert!(
            rows[4].starts_with("│ alpha finished the refactor"),
            "{rows:#?}"
        );
        assert!(rows[4].ends_with(" │"));
        assert_eq!(rows[4].chars().count(), w);
        assert_eq!(rows[5], format!("╰{}╯", "─".repeat(w - 2)));
        assert_eq!(rows[6].chars().count(), w);
        assert!(rows[6].starts_with("╭─ nobody ─"), "{rows:#?}");
        assert_eq!(
            rows[7].trim_end(),
            format!("│ who is that{} │", " ".repeat(w - 15))
        );
        assert!(rows[8].starts_with('╰'));
        assert!(
            !screen(&lines).contains("<thread") && !screen(&lines).contains("</thread>"),
            "{rows:#?}"
        );
        let (y3, y6) = (a.y + 3, a.y + 6);
        let color = thread_color("t-alpha");
        assert!(THREAD_COLORS.contains(&color));
        assert_eq!(buf[(a.x, y3)].fg, color);
        assert_eq!(buf[(a.x + 3, y3)].fg, color, "the name");
        assert_eq!(buf[(a.right() - 1, a.y + 4)].fg, color, "the side bar");
        assert_eq!(buf[(a.x + 2, a.y + 4)].fg, Color::Reset, "the text");
        assert_eq!(buf[(a.x, y6)].fg, Color::DarkGray, "no such thread");
        // Not a card: rounded, never the yellow of the orchestrator's box.
        assert_ne!(color, Color::Yellow);
        assert_eq!(buf[(a.x, y3)].symbol(), "╭");
    }

    /// A box written before its thread was renamed names it by its old name: it is still
    /// that thread's box, titled with its name now, in its color.
    #[test]
    fn a_box_under_a_former_name_is_its_threads() {
        let mut st = orch_reply(ALPHA_REPLY, false);
        let renamed = ThreadScript::new("t-alpha").event(EventBody::ThreadRenamed(
            twr_core::event::ThreadRenamed {
                name: "alpha-v2".into(),
                previous: "alpha".into(),
                ..Default::default()
            },
        ));
        st.registry.apply(&renamed);
        let buf = crate::app::tests::draw(&mut st, 120, 30);
        let lines: Vec<String> = (0..30)
            .map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let rows = transcript_rows(&st, &lines);
        assert!(rows[3].starts_with("╭─ alpha-v2 · running ─"), "{rows:#?}");
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        assert_eq!(buf[(a.x, a.y + 3)].fg, thread_color("t-alpha"));
    }

    /// Every thread has a color of its own palette, the same every time; the palette has
    /// none of the orchestrator's cyan, a dive's magenta or the working cursor's orange.
    #[test]
    fn thread_colors_are_stable_and_apart_from_the_other_accents() {
        assert_eq!(thread_color("t-alpha"), thread_color("t-alpha"));
        let used: HashSet<String> = (0..64)
            .map(|i| format!("{:?}", thread_color(&format!("t-{i}"))))
            .collect();
        assert!(used.len() > 3, "{used:?}");
        for c in [
            Color::Indexed(80),
            Color::Indexed(176),
            icon::ACCENT,
            Color::Cyan,
            Color::Magenta,
            Color::Yellow,
        ] {
            assert!(!THREAD_COLORS.contains(&c), "{c:?}");
        }
    }

    /// At any width a box's rows fit the pane, its text wrapped inside; below
    /// [`BOX_MIN_WIDTH`] the section is its name, then its text, with no frame. Nothing
    /// panics, down to no width at all.
    #[test]
    fn a_thread_box_wraps_inside_at_any_width() {
        let st = orch_reply(ALPHA_REPLY, false);
        let entries = st.transcript.as_ref().unwrap().entries.clone();
        let threads = Threads {
            boxes: true,
            state: Some(&st),
        };
        let view = |w: u16| transcript_view(&entries, w, &Collapse::default(), &threads);
        let rows = view(20);
        let text: Vec<String> = rows
            .iter()
            .filter(|r| r.3 == Edge::Bars)
            .map(|r| r.0.to_string())
            .collect();
        assert_eq!(
            text[..4],
            [
                "│ alpha finished   │",
                "│ the refactor and │",
                "│ every test       │",
                "│ passes on the    │",
            ]
        );
        // No room for the status: the name alone.
        let top = format!("╭─ alpha {}╮", "─".repeat(10));
        assert!(rows.iter().any(|r| r.0.to_string() == top), "{rows:#?}");
        for w in 0..=40u16 {
            for (line, _, _, edge) in view(w) {
                let cells = line.width();
                if matches!(edge, Edge::None | Edge::Gutter(_))
                    || (edge == Edge::Bottom && usize::from(w) < BOX_MIN_WIDTH)
                {
                    assert!(cells <= usize::from(w.max(1)), "{w}: {line}");
                } else {
                    assert_eq!(cells, usize::from(w), "{w}: {line}");
                    assert!(usize::from(w) >= BOX_MIN_WIDTH);
                }
            }
        }
        let narrow: Vec<String> = view(7).iter().map(|r| r.0.to_string()).collect();
        assert!(narrow.contains(&"alpha".to_string()), "{narrow:#?}");
        assert!(!narrow.iter().any(|r| r.contains('│') || r.contains('<')));
        // A worker's text shows as it is.
        let raw = Threads {
            boxes: false,
            state: Some(&st),
        };
        let rows = transcript_view(&entries, 60, &Collapse::default(), &raw);
        let shown: Vec<String> = rows.iter().map(|r| r.0.to_string()).collect();
        assert!(
            shown.iter().any(|r| r == "  <thread name=\"alpha\">"),
            "{shown:#?}"
        );
    }

    /// A drag across a box copies its title and its text, never its frame: the wrapped text
    /// is one line again.
    #[test]
    fn a_copy_of_a_thread_box_leaves_out_its_frame() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut st = orch_reply(ALPHA_REPLY, false);
        let now = std::time::Instant::now();
        let buf = draw_offscreen(&mut st, 60, 30, now);
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let left = MouseButton::Left;
        let row = |y: u16| -> String { (a.x..a.right()).map(|x| buf[(x, y)].symbol()).collect() };
        let top = (a.y..a.bottom())
            .find(|&y| row(y).starts_with("╭─ alpha"))
            .unwrap();
        let bottom = (top..a.bottom())
            .find(|&y| row(y).starts_with('╰'))
            .unwrap();
        assert!(bottom > top + 2, "the text wraps at 60 columns");
        assert!(st
            .handle_mouse(mouse(MouseEventKind::Down(left), a.x, top), &buf)
            .is_empty());
        assert!(st
            .handle_mouse(
                mouse(MouseEventKind::Drag(left), a.right() - 1, bottom),
                &buf
            )
            .is_empty());
        let actions = st.handle_mouse(mouse(MouseEventKind::Up(left), a.right() - 1, bottom), &buf);
        let [crate::app::Action::Copy { text }] = &actions[..] else {
            panic!("{actions:?}");
        };
        assert_eq!(
            text,
            "alpha · running\nalpha finished the refactor and every test passes on the first run."
        );
    }

    /// Draws the orchestrator reply `text` at 60 × 30 and copies it with a drag from the first
    /// row starting with `from` to the end of the last row starting with `to`.
    fn copy_reply(text: &str, from: &str, to: &str) -> String {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut st = orch_reply(text, false);
        let buf = draw_offscreen(&mut st, 60, 30, std::time::Instant::now());
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let left = MouseButton::Left;
        let row = |y: u16| -> String { (a.x..a.right()).map(|x| buf[(x, y)].symbol()).collect() };
        let top = (a.y..a.bottom())
            .find(|&y| row(y).trim_start().starts_with(from))
            .unwrap();
        let end = (a.y..a.bottom())
            .rev()
            .find(|&y| row(y).contains(to))
            .unwrap();
        st.handle_mouse(mouse(MouseEventKind::Down(left), a.x, top), &buf);
        st.handle_mouse(mouse(MouseEventKind::Drag(left), a.right() - 1, end), &buf);
        let actions = st.handle_mouse(mouse(MouseEventKind::Up(left), a.right() - 1, end), &buf);
        let [crate::app::Action::Copy { text }] = &actions[..] else {
            panic!("{actions:?}");
        };
        text.clone()
    }

    /// A box's bottom border is all frame: a copy across adjacent boxes and the text after
    /// them has no blank line where a border was, while a blank line of text, in a box or
    /// outside one, is still copied.
    #[test]
    fn a_copy_across_boxes_has_no_blank_line_for_their_borders() {
        let adjacent = "<thread name=\"alpha\">\none\n</thread>\n<thread name=\"beta\">\ntwo\n\
            </thread>\nafter";
        assert_eq!(
            copy_reply(adjacent, "╭─ alpha", "after"),
            "alpha · running\none\nbeta · running\ntwo\nafter"
        );
        // Starting on a bottom border adds no line break either.
        assert_eq!(
            copy_reply(adjacent, "╰", "after"),
            "beta · running\ntwo\nafter"
        );
        let blank = "<thread name=\"alpha\">\none\n\ntwo\n</thread>\n\nafter";
        assert_eq!(
            copy_reply(blank, "╭─ alpha", "after"),
            "alpha · running\none\n\ntwo\n\nafter"
        );
    }

    /// The conversation reads like Claude Code's: the prompt after `❯`, each reply after `●`
    /// with its markdown drawn, tool calls as one line saying what they did, answered requests
    /// one row each (a run of them one row), a report as a block, the end of a long turn after
    /// `✻`, and a blank row between blocks.
    #[test]
    fn the_conversation_reads_like_claude_code() {
        let tool = |id: &str, name: &str, summary: &str| Entry::Tool {
            call_id: id.into(),
            tool: name.into(),
            summary: summary.into(),
            ok: Some(true),
            thread: None,
        };
        let update = |text: &str, id: Option<&str>, outcome: Option<&str>| Entry::HubUpdate {
            text: text.into(),
            user_message: None,
            attn_id: id.map(str::to_string),
            about: None,
            outcome: outcome.map(str::to_string),
            seq: 0,
            resolution: false,
        };
        let mut entries = vec![
            Entry::TurnStart {
                seq: 0,
                turn: Some(1),
                from: TurnFrom::User,
                prompt: "check `api`".into(),
                notice: false,
                withdrawn: false,
            },
            Entry::Text {
                seq: 0,
                text: "## Plan\n- **one** step".into(),
                open: false,
            },
            tool("c1", "Bash", "ls"),
            tool("c2", "Bash", "cat x"),
            tool("c3", "Read", "x"),
            Entry::Text {
                seq: 0,
                text: "done".into(),
                open: false,
            },
            Entry::TurnEnd {
                result: TurnResult::Completed,
                duration_ms: Some(75_000),
            },
        ];
        for k in 0..3 {
            entries.push(update(
                &format!("api needs permission: Allow shell: step {k}"),
                Some(&format!("a{k}")),
                Some("answered: allow"),
            ));
        }
        entries.push(update(
            "api needs permission: Allow shell: rm",
            Some("a9"),
            Some("answered: deny"),
        ));
        entries.push(update("api finished (success): All **green**.", None, None));
        let collapse = Collapse {
            on: true,
            quiet_ends: true,
            ..Default::default()
        };
        let rows: Vec<String> = transcript_view(&entries, 60, &collapse, &Threads::default())
            .iter()
            .map(|r| r.0.to_string().trim_end().to_string())
            .collect();
        assert_eq!(
            rows,
            [
                "❯ check `api`",
                "",
                "● Plan",
                "",
                "  - one step",
                "",
                "  Ran 2 shell commands, read 1 file (ctrl+o to expand)",
                "",
                "● done",
                "",
                "✻ Worked for 1m 15s",
                "",
                "✗ 4 requests · 3 allowed, 1 denied · api (ctrl+o to expand)",
                "",
                "● api finished · success",
                "  All green.",
            ]
        );
        // Expanded, each request is a row: how it ended, the thread, what it asked.
        let open = Collapse {
            all: true,
            ..collapse
        };
        let rows: Vec<String> = transcript_view(&entries, 60, &open, &Threads::default())
            .iter()
            .map(|r| r.0.to_string().trim_end().to_string())
            .collect();
        assert!(
            rows.contains(&"✓ api allowed · Allow shell: step 0".to_string()),
            "{rows:#?}"
        );
        assert!(
            rows.contains(&"✗ api denied · Allow shell: rm".to_string()),
            "{rows:#?}"
        );
        assert!(rows.contains(&"● Bash(ls)".to_string()), "{rows:#?}");
        // A short completed turn ends quietly in the orchestrator conversation.
        let mut short = entries.clone();
        short[6] = Entry::TurnEnd {
            result: TurnResult::Completed,
            duration_ms: Some(2_000),
        };
        let rows = transcript_view(&short, 60, &collapse, &Threads::default());
        assert!(!rows.iter().any(|r| r.0.to_string().starts_with('✻')));
        assert_eq!(duration_text(3_200), "3.2s");
        assert_eq!(duration_text(3_725_000), "1h 2m");
    }

    /// The hub's updates say what happened, all of it: each line of a summary on a row of its
    /// own, a fold of ended requests counting how they ended, a question answered `closed` as
    /// an answer, a failure marked as one; no row is wider than the pane.
    #[test]
    fn hub_updates_say_what_happened() {
        let update = |text: &str, id: &str, outcome: Option<&str>| Entry::HubUpdate {
            text: text.into(),
            user_message: None,
            attn_id: Some(id.into()),
            about: None,
            outcome: outcome.map(str::to_string),
            seq: 0,
            resolution: false,
        };
        let show = |entries: &[Entry], w: u16, all: bool| -> Vec<String> {
            let c = Collapse {
                on: true,
                all,
                ..Default::default()
            };
            transcript_view(entries, w, &c, &Threads::default())
                .iter()
                .map(|r| r.0.to_string().trim_end().to_string())
                .collect()
        };
        let multi = [update(
            "api needs permission: Allow shell: cd /tmp\nrm -rf build",
            "a1",
            Some("answered: allow"),
        )];
        assert_eq!(
            show(&multi, 60, false),
            ["✓ api allowed · Allow shell: cd /tmp", "  rm -rf build"]
        );
        let closed: Vec<Entry> = (0..3)
            .map(|k| {
                update(
                    "api asks: Open or closed?",
                    &format!("q{k}"),
                    Some("closed"),
                )
            })
            .collect();
        assert_eq!(
            show(&closed, 60, false),
            ["· 3 requests · 3 closed · api (ctrl+o to expand)"]
        );
        let answered = [update(
            "api asks: Open or closed?",
            "q9",
            Some("answered: closed"),
        )];
        assert_eq!(
            show(&answered, 60, false),
            ["✓ api answered closed · Open or closed?"]
        );
        let lines = [update(
            "api asks: Which?",
            "q8",
            Some("answered: first\nsecond"),
        )];
        assert_eq!(
            show(&lines, 60, false),
            ["✓ api answered first", "  second · Which?"]
        );
        // Two runs that share an attention id fold under keys of their own.
        let mut runs = Vec::new();
        for (seq, id) in [
            (1, "a1"),
            (2, "a2"),
            (3, "a3"),
            (10, "a1"),
            (11, "a4"),
            (12, "a5"),
        ] {
            if seq == 10 {
                runs.push(Entry::Text {
                    seq: 0,
                    text: "between".into(),
                    open: false,
                });
            }
            let mut e = update("api needs permission: x", id, Some("answered: allow"));
            if let Entry::HubUpdate { seq: s, .. } = &mut e {
                *s = seq;
            }
            runs.push(e);
        }
        let c = Collapse {
            on: true,
            ..Default::default()
        };
        let keys: Vec<String> = transcript_view(&runs, 60, &c, &Threads::default())
            .into_iter()
            .filter_map(|r| r.2)
            .collect();
        assert_eq!(keys, ["answers:1", "answers:10"]);
        let failed = [update("api failed: boom", "e1", None)];
        assert_eq!(show(&failed, 60, false), ["✗ api failed: boom"]);
        for w in 1..=12u16 {
            for e in [&multi[..], &failed[..], &answered[..]] {
                for r in transcript_view(e, w, &Collapse::default(), &Threads::default()) {
                    assert!(r.0.width() <= usize::from(w), "{w}: {}", r.0);
                }
            }
        }
        // A prompt's first line keeps its indentation after the mark.
        let p = [Entry::TurnStart {
            seq: 0,
            turn: Some(1),
            from: TurnFrom::User,
            prompt: "    four\n    more".into(),
            notice: false,
            withdrawn: false,
        }];
        assert_eq!(show(&p, 40, false), ["❯     four", "      more"]);
    }

    /// A folded run of tool calls cut to the width still says what failed.
    #[test]
    fn a_cut_tool_line_keeps_its_failures() {
        let tool = |id: &str, name: &str, ok| Entry::Tool {
            call_id: id.into(),
            tool: name.into(),
            summary: String::new(),
            ok: Some(ok),
            thread: None,
        };
        let group = [
            tool("1", "Bash", true),
            tool("2", "Read", true),
            tool("3", "Edit", true),
            tool("4", "Glob", true),
            tool("5", "WebFetch", true),
            tool("6", "Write", false),
        ];
        let refs: Vec<&Entry> = group.iter().collect();
        let row = tools_row(&refs, None, &Threads::default(), 60).to_string();
        assert!(row.contains("1 failed (ctrl+o to expand)"), "{row}");
        assert!(crate::edit::width(&row) <= 60, "{row}");
    }

    /// A wrapped list item copies as one line, its marker and indentation left out of the
    /// gutter but the item's own `-` kept; inside a thread box too.
    #[test]
    fn a_wrapped_list_item_copies_as_one_line() {
        let item = "- a long item that has to wrap onto the next row and then some more";
        assert_eq!(copy_reply(item, "● -", "more"), item);
        // A reply that starts with a quote: the bullet takes the indentation's first cells
        // only, the row keeps its width and copies without the bar.
        let quote = format!("> {}", "q".repeat(30));
        let rows = transcript_view(
            &[Entry::Text {
                seq: 0,
                text: quote.clone(),
                open: false,
            }],
            36,
            &Collapse::default(),
            &Threads::default(),
        );
        assert_eq!(rows[0].0.to_string(), format!("● │ {}", "q".repeat(30)));
        assert_eq!(rows[0].3, Edge::Gutter(4));
        let boxed = format!("<thread name=\"alpha\">\n{item}\n</thread>");
        assert_eq!(
            copy_reply(&boxed, "╭─ alpha", "more"),
            format!("alpha · running\n{item}")
        );
    }

    /// A prompt reads as typed: backticks, a `#`, a fence or a list marker stay; an empty or
    /// blank-led prompt still draws its `❯`.
    #[test]
    fn a_prompt_reads_as_typed() {
        let rows = |prompt: &str| -> Vec<String> {
            let entries = [Entry::TurnStart {
                seq: 0,
                turn: Some(1),
                from: TurnFrom::User,
                prompt: prompt.into(),
                notice: false,
                withdrawn: false,
            }];
            transcript_view(&entries, 40, &Collapse::default(), &Threads::default())
                .iter()
                .map(|r| r.0.to_string().trim_end().to_string())
                .collect()
        };
        assert_eq!(rows("# not a heading"), ["❯ # not a heading"]);
        assert_eq!(rows("```"), ["❯ ```"]);
        assert_eq!(rows("- run `ls`"), ["❯ - run `ls`"]);
        assert_eq!(rows(""), ["❯"]);
        assert_eq!(rows("\nsecond"), ["❯", "  second"]);
        assert_eq!(rows("if x:\n    do_work()"), ["❯ if x:", "      do_work()"]);
        // The mark is left out of a copy, also on a blank first row.
        let entries = [Entry::TurnStart {
            seq: 0,
            turn: Some(1),
            from: TurnFrom::User,
            prompt: "\nsecond".into(),
            notice: false,
            withdrawn: false,
        }];
        let view = transcript_view(&entries, 40, &Collapse::default(), &Threads::default());
        assert_eq!(view[0].3, Edge::Gutter(2));
        // One the orchestrator sent says so on a row above it that a copy leaves out entirely.
        for w in [12u16, 24, 40] {
            let entries = [Entry::TurnStart {
                seq: 0,
                turn: Some(1),
                from: TurnFrom::Orchestrator,
                prompt: "do this task".into(),
                notice: false,
                withdrawn: false,
            }];
            let view = transcript_view(&entries, w, &Collapse::default(), &Threads::default());
            assert!(view[0].0.to_string().starts_with("from the"), "{w}");
            assert_eq!((view[0].1, view[0].3), (Join::Skip, Edge::Bottom));
            assert!(view[1].0.to_string().starts_with("❯ do"), "{w}");
        }
    }

    /// Text wraps by display width, between grapheme clusters: a wide character with no room
    /// left starts the next row, a base keeps its combining marks, an emoji sequence stays
    /// whole.
    #[test]
    fn text_wraps_by_display_width_between_clusters() {
        assert_eq!(
            wrap_rows("日本語abcd", 6),
            [
                ("日本語".to_string(), Join::Newline),
                ("abcd".into(), Join::Glue)
            ]
        );
        assert_eq!(wrap("日本語", 5), ["日本", "語"]);
        assert_eq!(wrap("ab 日本", 4), ["ab", "日本"]);
        assert_eq!(
            wrap("e\u{301}e\u{301}e\u{301}", 2),
            ["e\u{301}e\u{301}", "e\u{301}"]
        );
        assert_eq!(wrap("👨‍👩‍👧👍🏽x", 3), ["👨‍👩‍👧", "👍🏽x"]);
        for w in 1..=12 {
            for row in wrap("漢字 and 😀😀 cafe\u{301} 🇯🇵🇯🇵🇯🇵", w) {
                assert!(crate::edit::width(&row) <= w.max(2), "{w}: {row:?}");
            }
        }
        assert_eq!(truncate_cells("日本語", 4), "日…");
        assert_eq!(truncate_cells("日本語", 5), "日本…");
        assert_eq!(truncate_cells("日本語", 6), "日本語");
    }

    /// Draws a closed box for alpha around `body` in a `term_w` × 40 screen and copies it
    /// with a drag from its top-left corner to its bottom-right one. Returns the transcript
    /// pane's width, the box's rows as drawn and the copied text; `None` when the pane is too
    /// narrow for a box.
    fn copy_box(body: &str, term_w: u16) -> Option<(u16, Vec<String>, String)> {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut st = orch_reply(
            &format!("<thread name=\"alpha\">\n{body}\n</thread>"),
            false,
        );
        let now = std::time::Instant::now();
        draw_offscreen(&mut st, term_w, 40, now);
        // This helper tests copying the full box at each width, including widths where the
        // same text now needs the long-block preview opened first.
        let keys: Vec<_> = block_rows(&st)
            .iter()
            .filter_map(|r| r.2.as_deref().and_then(linked_block).map(str::to_string))
            .collect();
        st.view.long_expanded.extend(keys);
        let buf = draw_offscreen(&mut st, term_w, 40, now);
        let a = st.view.panes.get(&PaneId::Transcript)?.area;
        if usize::from(a.width) < BOX_MIN_WIDTH {
            return None;
        }
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let left = MouseButton::Left;
        // As a copy reads a row: a wide character's second cell is not one of its own.
        let row = |y: u16| -> String {
            let mut out = String::new();
            let mut x = a.x;
            while x < a.right() {
                let symbol = buf[(x, y)].symbol();
                out.push_str(symbol);
                x += u16::try_from(crate::edit::width(symbol).max(1)).unwrap();
            }
            out
        };
        let top = (a.y..a.bottom())
            .find(|&y| row(y).starts_with("╭─ "))
            .unwrap_or_else(|| panic!("{term_w}: no box"));
        let bottom = (top..a.bottom())
            .find(|&y| row(y).starts_with('╰'))
            .unwrap_or_else(|| panic!("{term_w}: no bottom"));
        let rows = (top..=bottom).map(row).collect();
        st.handle_mouse(mouse(MouseEventKind::Down(left), a.x, top), &buf);
        st.handle_mouse(
            mouse(MouseEventKind::Drag(left), a.right() - 1, bottom),
            &buf,
        );
        let actions = st.handle_mouse(mouse(MouseEventKind::Up(left), a.right() - 1, bottom), &buf);
        let [crate::app::Action::Copy { text }] = &actions[..] else {
            panic!("{term_w}: {actions:?}");
        };
        Some((a.width, rows, text.clone()))
    }

    /// A box wraps, pads and cuts by display width: wide characters (CJK, emoji) take two
    /// cells and combining marks none, every framed row is exactly the pane's width, a wide
    /// character with no room at a row's end starts the next row, and a copy is the text.
    #[test]
    fn a_thread_box_wraps_wide_text_by_display_width() {
        let bodies = [
            "日本語abcd",
            "日本語のテキスト は 幅が 二つ",
            "ok 👍🏽 done 🎉🎉🎉 family 👨‍👩‍👧 flag 🇯🇵",
            "cafe\u{301} nai\u{308}ve e\u{301}\u{301}\u{301}x Zalgo",
            "mixed 漢字abc😀def 中文 wörds",
        ];
        let threads_state = orch_reply("", false);
        let threads = Threads {
            boxes: true,
            state: Some(&threads_state),
        };
        for body in bodies {
            let entries = vec![Entry::Text {
                seq: 0,
                text: format!("<thread name=\"日本語の長い名前alpha\">\n{body}\n</thread>"),
                open: false,
            }];
            for w in 8..=40u16 {
                let rows = transcript_view(&entries, w, &Collapse::default(), &threads);
                assert!(rows.iter().any(|r| r.3 == Edge::Bars), "{w}: {rows:#?}");
                for (line, _, _, edge) in &rows {
                    if *edge != Edge::None {
                        assert_eq!(line.width(), usize::from(w), "{w} {body:?}: {line}");
                        assert_eq!(crate::edit::width(&line.to_string()), usize::from(w));
                    }
                }
            }
        }
        // The reviewer's case at a pane 10 cells wide: `日本語` fills the 6 inner cells and
        // `abcd` starts the next row.
        let mut seen = std::collections::BTreeSet::new();
        for term_w in 8..=80u16 {
            for body in bodies {
                let Some((w, rows, copied)) = copy_box(body, term_w) else {
                    continue;
                };
                seen.insert(w);
                for r in &rows {
                    assert_eq!(crate::edit::width(r), usize::from(w), "{w}: {rows:#?}");
                }
                let (_, text) = copied
                    .split_once('\n')
                    .unwrap_or_else(|| panic!("{copied:?}"));
                assert_eq!(text, body, "{w}: {rows:#?}");
                if w == 10 && body == bodies[0] {
                    assert_eq!(rows[1], "│ 日本語 │", "{rows:#?}");
                    assert_eq!(rows[2], "│ abcd   │", "{rows:#?}");
                }
            }
        }
        assert!((8..=40).all(|w| seen.contains(&w)), "{seen:?}");
    }

    /// While a reply streams into a box, the box is open at the bottom and the cursor waits
    /// inside it, after the text.
    #[test]
    fn a_box_streaming_in_is_open_with_the_cursor_inside() {
        let mut st = orch_working(vec![
            prompt("news?"),
            Entry::Text {
                seq: 0,
                text: "<thread name=\"alpha\">\nalpha is still at it".into(),
                open: true,
            },
        ]);
        let rows = drawn_rows(&mut st);
        assert!(rows[2].starts_with("╭─ alpha · running"), "{rows:#?}");
        assert!(rows[3].starts_with("│ alpha is still at it▅ "), "{rows:#?}");
        assert!(rows[3].ends_with(" │"), "{rows:#?}");
        assert!(!rows.iter().any(|r| r.starts_with('╰')), "{rows:#?}");
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap();
        let cursor = Rect::new(pane.area.x + 22, pane.area.y + 3, 1, 1);
        assert!(pane.skip.contains(&cursor), "{:?}", pane.skip);
        // Just opened, with no text yet: the cursor on the box's first row.
        st.transcript.as_mut().unwrap().entries[1] = Entry::Text {
            seq: 0,
            text: "<thread name=\"alpha\">\n".into(),
            open: true,
        };
        let rows = drawn_rows(&mut st);
        assert!(rows[2].starts_with("╭─ alpha"), "{rows:#?}");
        assert!(
            rows[3].starts_with("│ ▅ ") && rows[3].ends_with(" │"),
            "{rows:#?}"
        );
        // Once finished, it is closed.
        st.transcript.as_mut().unwrap().entries[1] = Entry::Text {
            seq: 0,
            text: "<thread name=\"alpha\">\nalpha is done".into(),
            open: false,
        };
        let rows = drawn_rows(&mut st);
        assert!(rows[4].starts_with('╰'), "{rows:#?}");
    }

    fn long_body(n: usize) -> String {
        format!(
            "```\n{}\n```",
            (0..n)
                .map(|i| format!("row {i:03} 日本語 cafe\u{301}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    fn conversation_blocks(st: &AppState, width: u16) -> Vec<TranscriptRow> {
        transcript_view(
            st.transcript.as_ref().unwrap().visible().0,
            width,
            &Collapse {
                on: true,
                quiet_ends: true,
                all: st.view.expand_all,
                expanded: Some(&st.view.expanded),
            },
            &Threads {
                boxes: true,
                state: Some(st),
            },
        )
    }

    #[test]
    fn long_thread_blocks_have_identical_streaming_and_finished_previews() {
        let body = long_body(80);
        let mut st = orch_reply(&format!("<thread name=\"alpha\">\n{body}"), true);
        // Include narrow panes, below BOX_MIN_WIDTH, and phone-sized terminals.
        for width in [1, 7, 8, 28, 30, 43, 80] {
            let streaming = conversation_blocks(&st, width);
            let key = streaming
                .iter()
                .find_map(|r| r.2.as_deref().and_then(linked_block))
                .unwrap()
                .to_string();
            let block: Vec<_> = streaming
                .iter()
                .filter(|r| r.2.as_deref().and_then(linked_block) == Some(&key))
                .collect();
            assert_eq!(block.len(), BLOCK_MAX_HEIGHT, "width {width}");
            // The footer names what opens it, where it has room: a click (it is not
            // selected).
            let footer = block.last().unwrap().0.to_string();
            assert!(footer.contains("click") || width < 12, "{width}: {footer}");
            if width == 80 {
                assert!(block
                    .last()
                    .unwrap()
                    .0
                    .to_string()
                    .contains("… 70 more lines"));
            }
            assert!(streaming.iter().all(|r| r.0.width() <= usize::from(width)));
            if let Entry::Text { open, .. } = &mut st.transcript.as_mut().unwrap().entries[1] {
                *open = false;
            }
            assert_eq!(streaming, conversation_blocks(&st, width), "width {width}");
            st.view.long_expanded.insert(key.clone());
            let expanded = conversation_blocks(&st, width);
            assert!(expanded.len() > streaming.len());
            // A two-cell grapheme cannot fit in a one-cell pane (the wrapper emits it as
            // one row); normal and phone-sized panes must fit every row exactly.
            assert!(expanded
                .iter()
                .all(|r| r.0.width() <= usize::from(width.max(2))));
            if let Entry::Text { open, .. } = &mut st.transcript.as_mut().unwrap().entries[1] {
                *open = true;
            }
            assert_eq!(expanded, conversation_blocks(&st, width));
            st.view.long_expanded.remove(&key);
        }
    }

    #[test]
    fn long_plain_messages_prompts_reports_and_updates_are_expandable() {
        let body = "detail\n".repeat(100);
        let mut st = orch_reply(&long_body(100), false);
        let variants = [
            Entry::Text {
                seq: 21,
                text: long_body(100),
                open: false,
            },
            Entry::TurnStart {
                seq: 22,
                turn: Some(2),
                from: TurnFrom::User,
                prompt: body.clone(),
                notice: false,
                withdrawn: false,
            },
            Entry::Notice {
                seq: 23,
                level: NoticeLevel::Info,
                text: body.clone(),
            },
            Entry::HubUpdate {
                seq: 24,
                text: format!("alpha finished (success): {body}"),
                user_message: None,
                attn_id: None,
                about: None,
                outcome: None,
                resolution: false,
            },
            Entry::HubUpdate {
                seq: 25,
                text: body.clone(),
                user_message: Some(body.clone()),
                attn_id: None,
                about: None,
                outcome: None,
                resolution: false,
            },
            Entry::TurnStart {
                seq: 26,
                turn: Some(3),
                from: TurnFrom::Orchestrator,
                prompt: body,
                notice: true,
                withdrawn: false,
            },
        ];
        st.view.expand_all = true;
        for entry in variants {
            st.transcript.as_mut().unwrap().entries = vec![entry];
            let rows = conversation_blocks(&st, 30);
            assert_eq!(rows.len(), BLOCK_MAX_HEIGHT);
            let key = rows[0].2.clone().unwrap();
            assert!(rows.last().unwrap().0.to_string().contains("more"));
            assert!(rows.last().unwrap().0.to_string().contains('e'));
            st.view.long_expanded.insert(key);
            let full = conversation_blocks(&st, 30);
            assert!(full.len() > BLOCK_MAX_HEIGHT);
            assert!(full.last().unwrap().0.to_string().contains("collapse"));
        }
    }

    #[test]
    fn long_streaming_blocks_keep_the_same_height_and_cursor_layout_on_completion() {
        let body = long_body(60);
        for width in [30, 45, 120] {
            let mut st = orch_working(vec![
                prompt("report?"),
                Entry::Text {
                    seq: 301,
                    text: format!("<thread name=\"alpha\">\n{body}"),
                    open: true,
                },
            ]);
            st.view.sidebar_hidden = true;
            let now = std::time::Instant::now();
            for expanded in [false, true] {
                if let Entry::Text { open, .. } = &mut st.transcript.as_mut().unwrap().entries[1] {
                    *open = true;
                }
                if expanded {
                    let key = conversation_blocks(&st, width - 2)
                        .iter()
                        .find_map(|r| r.2.as_deref().and_then(linked_block))
                        .unwrap()
                        .to_string();
                    st.view.long_expanded.insert(key);
                }
                let streaming = draw_offscreen(&mut st, width, 30, now);
                let pane = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
                if let Entry::Text { open, .. } = &mut st.transcript.as_mut().unwrap().entries[1] {
                    *open = false;
                }
                let finished = draw_offscreen(&mut st, width, 30, now);
                let after = st.view.panes.get(&PaneId::Transcript).unwrap();
                assert_eq!(pane.max_scroll, after.max_scroll);
                for y in pane.area.y..pane.area.bottom() {
                    for x in pane.area.x..pane.area.right() {
                        assert_eq!(streaming[(x, y)], finished[(x, y)], "{width}: ({x}, {y})");
                    }
                }
            }
        }
    }

    #[test]
    fn block_keys_toggling_scrolling_and_resize_preserve_individual_choices() {
        let body = long_body(60);
        let text = format!("<thread name=\"alpha\">\n{body}\n</thread>\n<thread name=\"alpha\">\n{body}\n</thread>");
        let mut st = orch_reply(&text, true);
        if let Entry::Text { seq, .. } = &mut st.transcript.as_mut().unwrap().entries[1] {
            *seq = 42;
        }
        st.view.sidebar_hidden = true;
        let shift = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::SHIFT);
        let now = std::time::Instant::now();
        draw_offscreen(&mut st, 40, 30, now);
        // The conversation selected, Shift-↑ selects the last block in view, Enter toggles it.
        st.handle_key(shift(KeyCode::Left));
        st.handle_key(shift(KeyCode::Up));
        st.handle_key(crate::app::tests::key(KeyCode::Enter));
        let second = st.view.block.clone().unwrap();
        assert!(st.view.long_expanded.contains(&second));
        let buf = draw_offscreen(&mut st, 40, 30, now);
        assert!(
            screen(&buffer_lines(&buf)).contains("row 000"),
            "expansion opens at the head"
        );
        st.handle_key(crate::app::tests::key(KeyCode::PageDown));
        let scrolled = st.view.scroll;
        draw_offscreen(&mut st, 40, 30, now);
        st.handle_key(crate::app::tests::key(KeyCode::PageDown));
        assert!(st.view.scroll < scrolled);
        st.handle_key(shift(KeyCode::Up));
        let first = st.view.block.clone().unwrap();
        assert_ne!(
            first, second,
            "identical names/content still identify separate sections"
        );
        draw_offscreen(&mut st, 40, 30, now);
        st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert_eq!(st.view.long_expanded.len(), 2);
        for width in [30, 45, 120, 35] {
            draw_offscreen(&mut st, width, 30, now);
            assert_eq!(st.view.long_expanded.len(), 2);
        }
        st.handle_key(shift(KeyCode::Down));
        assert_eq!(st.view.block.as_deref(), Some(second.as_str()));
        st.handle_key(shift(KeyCode::Up));
        assert_eq!(st.view.block.as_deref(), Some(first.as_str()));
        // Removing older entries never reassigns the real event/section keys.
        st.transcript.as_mut().unwrap().entries.remove(0);
        if let Entry::Text { open, .. } = &mut st.transcript.as_mut().unwrap().entries[0] {
            *open = false;
        }
        draw_offscreen(&mut st, 35, 30, now);
        assert!(conversation_blocks(&st, 33)
            .iter()
            .any(|r| r.2.as_deref() == Some(&first)));
        st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert!(!st.view.long_expanded.contains(&first));
        assert!(st.view.long_expanded.contains(&second));
        st.dive("t-alpha");
        st.back();
        assert!(st.view.long_expanded.contains(&second));
        // A letter types, whatever is selected; Enter then sends rather than toggles.
        st.handle_key(crate::app::tests::key(KeyCode::Char('e')));
        assert_eq!(st.view.input.as_str(), "e");
    }

    fn buffer_lines(buf: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    /// A copy from the footer of a block that cuts a long line, down through the text after
    /// it, is that text alone: the footer adds nothing, not even a line break.
    #[test]
    fn a_copy_from_a_footer_cutting_a_line_starts_with_the_text_after_it() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let line = (0..300)
            .map(|k| format!("lq{k}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut st = orch_reply(
            &format!("<thread name=\"alpha\">\n{line}\n</thread>\nafter"),
            false,
        );
        st.view.sidebar_hidden = true;
        let footer_join = conversation_blocks(&st, 38)
            .iter()
            .find(|r| {
                r.2.as_deref()
                    .is_some_and(|l| l.starts_with("toggle:block:"))
            })
            .map(|r| r.1);
        assert_eq!(footer_join, Some(Join::Glue), "the footer cuts the line");
        let now = std::time::Instant::now();
        let buf = draw_offscreen(&mut st, 40, 30, now);
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let footer = pane
            .links
            .iter()
            .position(|l| l.as_deref().is_some_and(|l| l.starts_with("toggle:block:")))
            .unwrap() as u16
            + pane.area.y;
        let after = (footer..pane.area.bottom())
            .find(|&y| {
                (pane.area.x..pane.area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("after")
            })
            .unwrap();
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let left = MouseButton::Left;
        let right = pane.area.right() - 1;
        st.handle_mouse(mouse(MouseEventKind::Down(left), pane.area.x, footer), &buf);
        st.handle_mouse(mouse(MouseEventKind::Drag(left), right, after), &buf);
        let copied = st.handle_mouse(mouse(MouseEventKind::Up(left), right, after), &buf);
        let [crate::app::Action::Copy { text }] = &copied[..] else {
            panic!("{copied:?}");
        };
        assert_eq!(text, "after");
    }

    #[test]
    fn long_block_footer_clicks_toggle_and_drags_skip_the_affordance() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut st = orch_reply(
            &format!("<thread name=\"alpha\">\n{}\n</thread>", long_body(80)),
            false,
        );
        st.view.sidebar_hidden = true;
        let now = std::time::Instant::now();
        let buf = draw_offscreen(&mut st, 40, 30, now);
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let footer = pane
            .links
            .iter()
            .position(|l| l.as_deref().is_some_and(|l| l.starts_with("toggle:block:")))
            .unwrap() as u16
            + pane.area.y;
        let mouse = |kind, row| MouseEvent {
            kind,
            column: pane.area.x,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let left = MouseButton::Left;
        st.handle_mouse(mouse(MouseEventKind::Down(left), pane.area.y), &buf);
        st.handle_mouse(mouse(MouseEventKind::Drag(left), footer), &buf);
        let copied = st.handle_mouse(mouse(MouseEventKind::Up(left), footer), &buf);
        let [crate::app::Action::Copy { text }] = &copied[..] else {
            panic!("{copied:?}");
        };
        assert!(!text.contains("more lines") && !text.contains("expand"));
        st.handle_mouse(mouse(MouseEventKind::Down(left), footer), &buf);
        st.handle_mouse(mouse(MouseEventKind::Up(left), footer), &buf);
        assert_eq!(st.view.long_expanded.len(), 1);
        let key = st.view.block.clone().unwrap();
        st.view.scroll = 0;
        let buf = draw_offscreen(&mut st, 40, 30, now);
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let footer = pane
            .links
            .iter()
            .position(|l| l.as_deref() == Some(&format!("toggle:{key}")))
            .unwrap() as u16
            + pane.area.y;
        let mouse = |kind| MouseEvent {
            kind,
            column: pane.area.x,
            row: footer,
            modifiers: KeyModifiers::NONE,
        };
        st.handle_mouse(mouse(MouseEventKind::Down(left)), &buf);
        st.handle_mouse(mouse(MouseEventKind::Up(left)), &buf);
        assert!(st.view.long_expanded.is_empty());
    }

    fn twr_call(id: &str, tool: &str, summary: &str, thread: Option<&str>) -> Entry {
        Entry::Tool {
            call_id: id.into(),
            tool: format!("mcp__twr__{tool}"),
            summary: summary.into(),
            ok: Some(true),
            thread: thread.map(str::to_string),
        }
    }

    fn notice(turn: u64, lines: &[&str]) -> Entry {
        Entry::TurnStart {
            seq: 0,
            turn: Some(turn),
            from: TurnFrom::Orchestrator,
            prompt: format!(
                "[twr notice] updates.\n{}\nThe user does not see this.",
                lines.join("\n")
            ),
            notice: true,
            withdrawn: false,
        }
    }

    /// A folded group whose calls are all about one thread names it after the dot, in the
    /// thread's color; so does a folded notice whose updates are all about one. A group or a
    /// notice about several threads, or with a call about none, is not named. An answer to an
    /// item names the item's thread: open, or named next to its id in a notice.
    #[test]
    fn a_folded_line_about_one_thread_names_it() {
        let mut st = crate::app::tests::without_worker_requests(crate::app::tests::with_orch());
        st.transcript.as_mut().unwrap().entries = vec![
            notice(1, &["- thread=beta reason=permission attn_id=pm9 options=[allow, deny] summary=\"x\""]),
            twr_call("c1", "send", "{}", Some("alpha")),
            twr_call("c2", "respond_attention", r#"{"answer":"retry","attn_id":"rv1"}"#, None),
            Entry::Text {
                seq: 0,
                text: "one".into(),
                open: false,
            },
            twr_call("c3", "send", "{}", Some("t-alpha")),
            twr_call("c4", "set_mode", "{}", Some("beta")),
            Entry::Text {
                seq: 0,
                text: "two".into(),
                open: false,
            },
            twr_call("c5", "list_threads", "{}", None),
            twr_call("c6", "send", "{}", Some("alpha")),
            Entry::Text {
                seq: 0,
                text: "three".into(),
                open: false,
            },
            twr_call("c7", "respond_attention", r#"{"answer":"allow","attn_id":"pm9"}"#, None),
            notice(2, &["- thread=gamma report turn=1 grade=success summary=\"y\"", "- thread=alpha report turn=2 grade=success summary=\"z\""]),
            notice(3, &["- thread=nobody report turn=1 grade=success summary=\"y\""]),
        ];
        let buf = crate::app::tests::draw(&mut st, 120, 30);
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let row = |y: u16| -> String {
            (a.x..a.right())
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        let all: Vec<(u16, String)> = (a.y..a.bottom()).map(|y| (y, row(y))).collect();
        let rows: Vec<&str> = all
            .iter()
            .map(|(_, r)| r.as_str())
            .filter(|r| !r.is_empty())
            .collect();
        assert_eq!(
            rows,
            [
                "  beta · hub update (ctrl+o to expand)",
                "  alpha · Sent 1 message, answered 1 request (ctrl+o to expand)",
                "● one",
                "  Sent 1 message, changed 1 mode (ctrl+o to expand)",
                "● two",
                "  Listed the threads, sent 1 message (ctrl+o to expand)",
                "● three",
                "  beta · Answered 1 request (ctrl+o to expand)",
                "  hub update (ctrl+o to expand)",
                "  nobody · hub update (ctrl+o to expand)",
            ]
        );
        let y = |start: &str| all.iter().find(|(_, r)| r.starts_with(start)).unwrap().0;
        let alpha = y("  alpha");
        assert_eq!(buf[(a.x + 2, alpha)].fg, thread_color("t-alpha"));
        assert_eq!(buf[(a.x + 2, y("  beta"))].fg, thread_color("t-beta"));
        assert_eq!(buf[(a.x + 8, alpha)].fg, Color::DarkGray, "the rest is dim");
        assert_eq!(
            buf[(a.x + 2, y("  nobody"))].fg,
            Color::DarkGray,
            "no such thread"
        );
        assert_eq!(
            st.view.panes.get(&PaneId::Transcript).unwrap().link(alpha),
            Some("tools:c1")
        );
    }
    #[test]
    fn background_cards_grid_and_status_line_stay_visible_and_working() {
        let mut state = AppState::new();
        let mut script = ThreadScript::new("background-worker");
        let events = [
            script.created("background-worker"),
            script.started("session"),
            script.turn_started("run tests"),
            script.event(EventBody::TaskUpdated(twr_core::event::TaskUpdated {
                task_id: "tests".into(),
                running: true,
                description: "cargo test".into(),
                ..Default::default()
            })),
            script.turn_ended(twr_core::event::TurnResult::Completed),
            script.report(Some(1), twr_core::event::Grade::Success),
        ];
        crate::app::tests::control(&mut state, &events);
        state.view.focus_mode = true;
        let worker = state.registry.get("background-worker").unwrap();
        assert!(icon::is_working(worker));
        assert!(!state.hides(worker));
        for screen_kind in [Screen::Stack, Screen::Grid] {
            state.view.screen = screen_kind;
            let output = screen(&draw(&state, 120, 40));
            assert!(output.contains("background (1 bg)"), "{output}");
            assert!(output.contains("1 background task(s)"), "{output}");
        }
        crate::app::tests::control(
            &mut state,
            &[
                script.event(EventBody::TaskUpdated(twr_core::event::TaskUpdated {
                    task_id: "tests".into(),
                    running: false,
                    ..Default::default()
                })),
            ],
        );
        assert!(!icon::is_working(
            state.registry.get("background-worker").unwrap()
        ));
    }

    /// At the same width, every transcript row is the row its anchor puts back
    /// ([`anchor_at`], [`anchor_row`]): rows of a long code line that hold only `+`, `=`, `/`,
    /// emoji or spaces, a list, a quote, a thread box, a failed tool call and the blank rows
    /// between them included.
    #[test]
    fn every_row_is_where_its_anchor_puts_it_back() {
        let code = format!(
            "let x = {}{}{}{}{}end;",
            "+".repeat(150),
            "=".repeat(150),
            "/".repeat(150),
            "🎉".repeat(60),
            " ".repeat(200),
        );
        let words = "word ".repeat(30);
        let entries = vec![
            prompt("hi"),
            Entry::Text {
                seq: 0,
                text: format!(
                    "Here:\n\n```\n{code}\n```\n\n- a list item {words}\n\n> a quote {words}"
                ),
                open: false,
            },
            Entry::Tool {
                call_id: "c1".into(),
                tool: "Bash".into(),
                summary: "x ".repeat(100),
                ok: Some(false),
                thread: None,
            },
            Entry::Text {
                seq: 0,
                text: format!("<thread name=\"web\">\n{words}\n</thread>\nafter"),
                open: false,
            },
        ];
        let ids: Vec<u64> = (10..10 + entries.len() as u64).collect();
        let threads = Threads {
            boxes: true,
            state: None,
        };
        let collapse = Collapse {
            on: true,
            all: true,
            ..Collapse::default()
        };
        for width in [20, 37, 80] {
            let (rows, spans) = transcript_view_spans(&entries, width, &collapse, &threads);
            let text: Vec<String> = rows
                .iter()
                .map(|r| r.0.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            for only in ["+", "=", "/", "🎉", " "] {
                assert!(
                    text.iter()
                        .any(|t| t.len() > 4 && t.trim_start().replace(only, "").trim().is_empty()),
                    "{width}: no row of {only:?} only"
                );
            }
            for (row, t) in text.iter().enumerate() {
                let a = anchor_at(&rows, &spans, &ids, &|_| None, row).unwrap();
                let back = anchor_row(&rows, &spans, &ids, &|_| None, &a);
                assert_eq!(
                    back,
                    (row as isize, true),
                    "{width}, row {row} {t:?}: {a:?}"
                );
            }
        }
    }
}
