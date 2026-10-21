//! The workspace sidebar: a card per workspace (`default` and every `twr ws` workspace) with
//! its orchestrator's status and the start of its latest reply ([`crate::workspace::Glance`]),
//! whether anything there works, and the counts the tab strip shows; then `+ New workspace`
//! and `⚙ Settings`. The current workspace's card is highlighted; a tap on a card switches to
//! that workspace, as its tab does.
//!
//! Where it goes ([`AppState::ws_docked`]): a column left of the views (stack, grid and the
//! decision table) in the full layout from [`DOCK_MIN_WIDTH`] columns, unless hidden (Ctrl-T,
//! `/spaces`, its header's `‹`, kept across runs). Narrower, the same list opens as a drawer
//! over the left of the view (Ctrl-T, `/spaces`, the tab strip's `≡`), and in the compact
//! layout it is the Workspaces screen. The tab strip is drawn only where the column is not,
//! so the workspaces never show twice.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use twr_core::reduce::ThreadStatus;

use crate::app::{AppState, WsRow};
use crate::icon;
use crate::select::{PaneId, Panes, Target};
use crate::workspace::{Glance, Tab, TabHub};

/// Width of the docked column, its border included.
pub const DOCK_WIDTH: u16 = 26;
/// Narrowest screen the column is docked on: the main pane and the cards beside it keep
/// their room.
pub const DOCK_MIN_WIDTH: u16 = 120;
/// Widest the drawer gets.
const DRAWER_MAX_WIDTH: u16 = 34;
/// Rows of a workspace's card: its name, its orchestrator's status, two of its reply, and a
/// blank row after it.
const CARD_ROWS: usize = 5;
/// Rows of a card's reply.
const PREVIEW_ROWS: usize = 2;

/// One row of the list as drawn: its line, and the row of the list it belongs to (a
/// workspace's card, a button; `None` for the arrows).
type ListRow = (Line<'static>, Option<WsRow>);

/// The docked column over `area` (its right column a border): its header, `WORKSPACES` and
/// the `‹` that hides it (the whole row), then the list ([`render_list`]).
pub fn render_docked(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    if area.width < 3 || area.height < 2 {
        return;
    }
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let head = Rect { height: 1, ..inner };
    let dim = Style::default().fg(Color::DarkGray);
    let room = usize::from(inner.width);
    let label = " WORKSPACES";
    let close = "‹ ";
    let gap = room.saturating_sub(crate::edit::width(label) + crate::edit::width(close));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(label, dim.add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(gap)),
            Span::styled(close, dim),
        ])),
        head,
    );
    panes.add(PaneId::Text, head);
    panes.target(head, Target::WsSidebar);
    let list = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    render_list(frame, list, state, panes, false);
}

/// The drawer over the left of `area` (Ctrl-T where the column does not fit): the list in a
/// box whose top border closes it. It covers what is under it.
pub fn render_drawer(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let width = area.width.min(DRAWER_MAX_WIDTH);
    let rect = Rect { width, ..area };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" workspaces ")
        .title(crate::ui::close_title());
    let inner = block.inner(rect);
    panes.cover(None);
    panes.drawer = Some(rect);
    crate::ui::close_row(panes, rect, Target::Close);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    render_list(frame, inner, state, panes, true);
}

/// The compact layout's Workspaces screen over `area`: the list as wide as the screen.
pub fn render_screen(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    render_list(frame, area, state, panes, true);
}

/// The list over `area`: the workspaces' cards ([`card_lines`]), scrolled to keep the one the
/// keys select in view (`keyed`: the drawer and the compact screen, where ↑↓ select) or as
/// the wheel left it, `↑ more` / `↓ more` at its edges when there is more; then the buttons
/// on its last two rows. Every row of a card is its tap target, and each button's row its own.
pub fn render_list(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    panes: &mut Panes,
    keyed: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = usize::from(area.width);
    let selected = keyed.then(|| state.ws_selected()).flatten();
    let buttons_h = 2.min(area.height);
    let cards_area = Rect {
        height: area.height - buttons_h,
        ..area
    };
    let mut rows: Vec<ListRow> = Vec::new();
    for tab in &state.tabs {
        let sel = selected.as_ref() == Some(&WsRow::Workspace(tab.name.clone()));
        for line in card_lines(state, tab, width, sel) {
            rows.push((line, Some(WsRow::Workspace(tab.name.clone()))));
        }
    }
    let fit = usize::from(cards_area.height);
    let max = rows.len().saturating_sub(fit);
    let mut start = state.view.ws_scroll.min(max);
    // The card the keys select, else the current one when nothing was scrolled, stays in view;
    // not once the wheel scrolled the list.
    let keep = selected
        .clone()
        .filter(|_| !state.view.ws_wheeled)
        .filter(|r| matches!(r, WsRow::Workspace(_)))
        .or_else(|| {
            (state.view.ws_scroll == 0 && !state.view.ws_wheeled)
                .then(|| state.workspace.clone().map(WsRow::Workspace))
                .flatten()
        });
    if let Some(keep) = keep {
        if let (Some(first), Some(last)) = (
            rows.iter().position(|(_, r)| r.as_ref() == Some(&keep)),
            rows.iter().rposition(|(_, r)| r.as_ref() == Some(&keep)),
        ) {
            if first < start {
                start = first;
            } else if last >= start + fit {
                start = (last + 1).saturating_sub(fit).min(first);
            }
        }
    }
    panes.ws_max = max;
    panes.ws_start = start;
    let mut shown: Vec<ListRow> = rows.iter().skip(start).take(fit).cloned().collect();
    let dim = Style::default().fg(Color::DarkGray);
    if start > 0 {
        if let Some(first) = shown.first_mut() {
            *first = (
                Line::from(Span::styled(format!("{:^width$}", "↑ more"), dim)),
                None,
            );
        }
    }
    if start + fit < rows.len() {
        if let Some(last) = shown.last_mut() {
            *last = (
                Line::from(Span::styled(format!("{:^width$}", "↓ more"), dim)),
                None,
            );
        }
    }
    panes.add(PaneId::Workspaces, cards_area);
    for (i, (_, row)) in shown.iter().enumerate() {
        if let Some(WsRow::Workspace(name)) = row {
            let y = cards_area.y + u16::try_from(i).unwrap_or(u16::MAX);
            panes.target(
                Rect::new(cards_area.x, y, cards_area.width, 1).intersection(cards_area),
                Target::Workspace(name.clone()),
            );
        }
    }
    // A moving icon on a card keeps the clock ticking.
    if state.view.animate {
        for (i, (_, row)) in shown.iter().enumerate() {
            let Some(WsRow::Workspace(name)) = row else {
                continue;
            };
            let first = rows.iter().position(|(_, r)| r.as_ref() == row.as_ref());
            if first == Some(start + i) && tab_active(state, name) {
                let y = cards_area.y + u16::try_from(i).unwrap_or(u16::MAX);
                panes.moves(Rect::new(cards_area.x + 1, y, 1, 1).intersection(cards_area));
            }
        }
    }
    frame.render_widget(
        Paragraph::new(shown.into_iter().map(|(l, _)| l).collect::<Vec<_>>()),
        cards_area,
    );
    let buttons = Rect {
        y: area.y + area.height - buttons_h,
        height: buttons_h,
        ..area
    };
    let entries = [
        (WsRow::New, "+ New workspace", Target::NewWorkspace),
        (WsRow::Settings, "⚙ Settings", Target::Settings),
    ];
    let mut lines = Vec::new();
    for (i, (row, text, target)) in entries.into_iter().enumerate().take(usize::from(buttons_h)) {
        let style = if selected.as_ref() == Some(&row) {
            crate::ui::selected_fill()
        } else {
            Style::default().fg(Color::Cyan)
        };
        let label = format!(" {text}");
        let pad = width.saturating_sub(crate::edit::width(&label));
        lines.push(Line::from(Span::styled(
            format!("{label}{}", " ".repeat(pad)),
            style,
        )));
        let y = buttons.y + u16::try_from(i).unwrap_or(u16::MAX);
        panes.target(Rect::new(buttons.x, y, buttons.width, 1), target);
    }
    panes.add(PaneId::Text, buttons);
    frame.render_widget(Paragraph::new(lines), buttons);
}

/// Whether workspace `name`'s card shows that something there works (its icon moves): its
/// hub runs, as [`card_lines`] has it.
pub(crate) fn tab_active(state: &AppState, name: &str) -> bool {
    state.tabs.iter().find(|t| t.name == name).is_some_and(|t| {
        matches!(state.tab_hub(t), TabHub::Running { .. })
            && state.tab_glance(t).is_some_and(|g| g.active)
    })
}

/// Workspace `tab`'s card, `width` columns wide, [`CARD_ROWS`] rows: its name with the icon
/// of what works there (`○` for a stopped hub) and its badges (`⚠2` waiting on you, `●3`
/// threads running); its orchestrator's status; the start of its latest reply, cut with `…`;
/// a blank row. The current workspace's name row is reversed, as its tab is, and its other
/// rows marked at their left; the one the keys select (`selected`) is filled as a selected
/// card is.
pub fn card_lines(state: &AppState, tab: &Tab, width: usize, selected: bool) -> Vec<Line<'static>> {
    let current = state.workspace.as_deref() == Some(tab.name.as_str());
    let hub = state.tab_hub(tab);
    let glance = state.tab_glance(tab);
    let dim = Style::default().fg(Color::DarkGray);
    let name_style = if selected {
        crate::ui::selected_fill().add_modifier(Modifier::BOLD)
    } else if current {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let mark = if current {
        Span::styled("▌", Style::default().fg(Color::Cyan))
    } else {
        Span::raw(" ")
    };
    let active = glance.as_ref().is_some_and(|g| g.active) && matches!(hub, TabHub::Running { .. });
    let lead = if active {
        icon::cursor(state.view.animate.then_some(state.view.frame))
    } else if matches!(hub, TabHub::Stopped) {
        Span::styled("○", dim)
    } else {
        Span::raw(" ")
    };
    let mut badges: Vec<Span<'static>> = Vec::new();
    if let TabHub::Running { running, .. } = hub {
        let waiting = hub.waiting();
        if waiting > 0 {
            badges.push(Span::styled(
                format!(" ⚠{waiting}"),
                name_style.fg(Color::Yellow),
            ));
        }
        if running > 0 {
            badges.push(Span::styled(format!(" ●{running}"), name_style));
        }
    }
    let badges_w: usize = badges.iter().map(|s| crate::edit::width(&s.content)).sum();
    // The mark, the icon, a space, the name, the badges, a space at the end.
    let room = width.saturating_sub(3 + badges_w + 1);
    let name = crate::ui::truncate_cells(&tab.name, room);
    let pad = room.saturating_sub(crate::edit::width(&name));
    let mut head = vec![
        mark.clone(),
        lead,
        Span::styled(format!(" {name}{}", " ".repeat(pad)), name_style),
    ];
    head.extend(badges);
    head.push(Span::styled(" ", name_style));
    let (status, status_style) = status_text(hub, glance.as_ref());
    let text_room = width.saturating_sub(3);
    let mut lines = vec![
        Line::from(head),
        Line::from(vec![
            mark.clone(),
            Span::raw("  "),
            Span::styled(crate::ui::truncate_cells(&status, text_room), status_style),
        ]),
    ];
    let preview = glance
        .as_ref()
        .and_then(|g| g.orch.as_ref())
        .and_then(|o| o.preview.as_deref())
        .map(|p| preview_rows(p, text_room, PREVIEW_ROWS))
        .unwrap_or_default();
    for k in 0..PREVIEW_ROWS {
        let text = preview.get(k).cloned().unwrap_or_default();
        lines.push(Line::from(vec![
            mark.clone(),
            Span::raw("  "),
            Span::styled(text, dim),
        ]));
    }
    lines.push(Line::default());
    debug_assert_eq!(lines.len(), CARD_ROWS);
    lines
}

/// What a card says of its workspace's orchestrator, and its style: `hub not running` for a
/// stopped hub, `not answering`, else its orchestrator's status: `running`, `waiting on you`
/// (yellow), `failed` (red), `idle`, or `no orchestrator`.
pub fn status_text(hub: TabHub, glance: Option<&Glance>) -> (String, Style) {
    let dim = Style::default().fg(Color::DarkGray);
    match hub {
        TabHub::Stopped => return ("hub not running".into(), dim),
        TabHub::Busy => return ("not answering".into(), dim),
        TabHub::Unknown if glance.is_none() => return ("…".into(), dim),
        _ => {}
    }
    let Some(orch) = glance.and_then(|g| g.orch.as_ref()) else {
        return ("no orchestrator".into(), dim);
    };
    match orch.status {
        ThreadStatus::Running => ("running".into(), Style::default().fg(Color::Green)),
        ThreadStatus::Background => (
            "running in the background".into(),
            Style::default().fg(Color::Green),
        ),
        ThreadStatus::Waiting => (
            "waiting on you".into(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        ThreadStatus::Error => ("failed".into(), Style::default().fg(Color::Red)),
        ThreadStatus::Attached => ("attached".into(), Style::default()),
        ThreadStatus::Parked => ("parked".into(), dim),
        ThreadStatus::Idle | ThreadStatus::Done => ("idle".into(), dim),
    }
}

/// The start of a reply as a card shows it: its prose run together (the thread boxes'
/// `<thread>` tags, Markdown's heading, quote and list marks, emphasis and code ticks left
/// out), wrapped to `width` on at most `max` rows, the last one cut with `…` when there is
/// more.
pub fn preview_rows(text: &str, width: usize, max: usize) -> Vec<String> {
    if width == 0 || max == 0 {
        return Vec::new();
    }
    // Lazily: only the words the rows take (and one more, to know whether to cut) are read.
    let mut words = text
        .lines()
        .map(str::trim)
        .filter(|l| {
            !(l.starts_with("<thread") || l.starts_with("</thread") || l.starts_with("```"))
        })
        .map(|l| l.trim_start_matches(['#', '>', '-', '*', '+', ' ']))
        .flat_map(str::split_whitespace)
        .map(|w| w.replace("**", "").replace('`', ""))
        .filter(|w| !w.is_empty())
        .peekable();
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut cut = false;
    for word in words.by_ref() {
        let w = crate::edit::width(&word);
        if row.is_empty() {
            row = crate::ui::truncate_cells(&word, width);
            if w > width && rows.len() + 1 == max {
                cut = true;
                break;
            }
        } else if crate::edit::width(&row) + 1 + w <= width {
            row.push(' ');
            row.push_str(&word);
        } else {
            rows.push(std::mem::take(&mut row));
            if rows.len() == max {
                cut = true;
                break;
            }
            row = crate::ui::truncate_cells(&word, width);
        }
    }
    if !row.is_empty() && rows.len() < max {
        rows.push(row);
    }
    if cut || words.peek().is_some() && rows.len() == max {
        if let Some(last) = rows.last_mut().filter(|l| !l.ends_with('…')) {
            let mut s = crate::ui::truncate_cells(last, width.saturating_sub(1));
            // (`truncate_cells` ends what it cut with `…` itself.)
            if !s.ends_with('…') {
                s.push('…');
            }
            *last = s;
        }
    }
    rows
}

/// What the new-workspace dialog's name follows.
const NAME_LABEL: &str = " name: ";

/// The new-workspace dialog, centered over `area`: the name typed so far, with the cursor
/// after it, and what is wrong with it (`twr ws new`'s rule, a workspace that exists) or how
/// to go on. Its top border closes it.
pub fn render_new(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let Some(name) = state.view.new_ws.as_deref() else {
        return;
    };
    let width = area.width.min(56);
    let inner_w = usize::from(width.saturating_sub(2)).max(1);
    let dim = Style::default().fg(Color::DarkGray);
    // Its end in view, the cursor after it, where the box is narrower than the name.
    let room = inner_w.saturating_sub(NAME_LABEL.len() + 1);
    let skip = name.chars().count().saturating_sub(room);
    let shown: String = name.chars().skip(skip).collect();
    let mut lines = vec![Line::from(vec![
        Span::raw(NAME_LABEL),
        Span::styled(shown.clone(), Style::default().add_modifier(Modifier::BOLD)),
    ])];
    let (note, style) = match state.new_ws_error(name) {
        Some(why) if !name.is_empty() => (why, Style::default().fg(Color::Red)),
        _ => (
            "a-z, 0-9 and - · Enter creates it and switches · Esc cancels".to_string(),
            dim,
        ),
    };
    lines.push(Line::default());
    lines.extend(crate::ui::wrap_line(
        Line::from(Span::styled(format!(" {note}"), style)),
        inner_w,
    ));
    let rect = crate::ui::centered(
        area,
        width,
        u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" new workspace ")
        .title(crate::ui::close_title());
    let inner = block.inner(rect);
    panes.cover(None);
    panes.add(PaneId::Text, inner);
    crate::ui::close_row(panes, rect, Target::Close);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
    // The cursor after the name, on its row.
    let label = u16::try_from(NAME_LABEL.len()).unwrap_or(u16::MAX);
    let x = inner.x + label + u16::try_from(crate::edit::width(&shown)).unwrap_or(u16::MAX);
    if inner.height > 0 && x < inner.x + inner.width {
        frame.set_cursor_position((x, inner.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{OrchGlance, Tab};

    /// The rows `st` draws at `w` x `h`, as the IO loop draws them (its panes kept).
    pub(crate) fn rows(st: &mut AppState, w: u16, h: u16) -> Vec<String> {
        let buf = crate::app::tests::draw(st, w, h);
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// A TUI on workspace `current` of `default`, `client-x` (running, its orchestrator
    /// waiting on the user, a reply), `scratch` (stopped, a reply seen before) and `idle`
    /// (running, nothing said yet).
    pub(crate) fn in_workspaces(current: &str) -> AppState {
        let mut st = crate::app::tests::with_orch();
        st.workspace = Some(current.into());
        st.instance_name = Some(current.into());
        st.view.compact_width = 60;
        let glance = |status, preview: Option<&str>, active| Glance {
            active,
            orch: Some(OrchGlance {
                status,
                preview: preview.map(str::to_string),
                ..Default::default()
            }),
        };
        st.set_tabs(vec![
            Tab {
                name: "default".into(),
                hub: TabHub::Unknown,
                glance: None,
            },
            Tab {
                name: "client-x".into(),
                hub: TabHub::Running {
                    threads: 2,
                    running: 1,
                    permissions: 1,
                    questions: 1,
                },
                glance: Some(glance(
                    ThreadStatus::Waiting,
                    Some("I need you to approve the deploy of the billing service to staging before I go on."),
                    true,
                )),
            },
            Tab {
                name: "scratch".into(),
                hub: TabHub::Stopped,
                glance: Some(glance(
                    ThreadStatus::Idle,
                    Some("Done: the parser handles the new format."),
                    false,
                )),
            },
            Tab {
                name: "docs".into(),
                hub: TabHub::Running {
                    threads: 0,
                    running: 0,
                    permissions: 0,
                    questions: 0,
                },
                glance: Some(glance(ThreadStatus::Idle, None, false)),
            },
        ]);
        st
    }

    /// The cell `x`, `y` of the last frame was a tap on: down and up there, a frame drawn
    /// before each, as the IO loop draws. Returns the actions.
    fn tap_at(st: &mut AppState, w: u16, h: u16, x: u16, y: u16) -> Vec<crate::app::Action> {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let ev = |kind| MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        let screen = crate::app::tests::draw(st, w, h);
        let mut out = st.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left)), &screen);
        let screen = crate::app::tests::draw(st, w, h);
        out.extend(st.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left)), &screen));
        out
    }

    /// Every cell of control `t` as a `w` x `h` frame draws it.
    fn cells(st: &mut AppState, w: u16, h: u16, t: &Target) -> Vec<Rect> {
        crate::app::tests::draw(st, w, h);
        st.view
            .panes
            .targets
            .iter()
            .filter(|(_, x)| x == t)
            .map(|(r, _)| *r)
            .collect()
    }

    /// A tap on control `t` (at its last cell) as a `w` x `h` frame draws it.
    fn tap(st: &mut AppState, w: u16, h: u16, t: &Target) -> Vec<crate::app::Action> {
        let r = *cells(st, w, h, t)
            .last()
            .unwrap_or_else(|| panic!("no {t:?} in {:?}", st.view.panes.targets));
        tap_at(st, w, h, r.x + r.width - 1, r.y + r.height - 1)
    }

    fn workspace_action(actions: &[crate::app::Action]) -> Option<(String, bool)> {
        actions.iter().find_map(|a| match a {
            crate::app::Action::Workspace { name, create } => Some((name.clone(), *create)),
            _ => None,
        })
    }

    /// From 120 columns the sidebar is docked left of the stack and the grid: a card per
    /// workspace with its orchestrator's status and reply, `○` for a stopped hub with the reply
    /// it last gave, the buttons under them; the tab strip is not drawn then (nor the
    /// instance label), so the workspaces show once.
    #[test]
    fn the_docked_sidebar_lists_every_workspace_once() {
        for screen in [crate::app::Screen::Stack, crate::app::Screen::Grid] {
            let mut st = in_workspaces("default");
            st.view.screen = screen;
            let r = rows(&mut st, 160, 40);
            let text = r.join("\n");
            assert!(st.ws_docked() && !st.tabs_shown(), "{text}");
            let col = |y: usize| {
                r[y].chars()
                    .take(usize::from(DOCK_WIDTH))
                    .collect::<String>()
            };
            assert!(
                col(0).contains("WORKSPACES") && col(0).contains('‹'),
                "{text}"
            );
            let left: Vec<String> = (0..40).map(col).collect();
            let left = left.join("\n");
            for want in [
                "default",
                "client-x        ⚠2 ●1",
                "waiting on you",
                "I need you to approve",
                "the deploy of the…",
                "○ scratch",
                "hub not running",
                "Done: the parser",
                "docs",
                "+ New workspace",
                "⚙ Settings",
            ] {
                assert!(left.contains(want), "{want:?} in\n{left}");
            }
            assert!(!text.contains("[default]"), "{text}");
            assert!(!text.contains("≡"), "{text}");
            // Each workspace once.
            assert_eq!(text.matches("client-x").count(), 1, "{text}");
            // Every row of a card is its target; the buttons' rows theirs.
            let card = cells(&mut st, 160, 40, &Target::Workspace("client-x".into()));
            assert_eq!(card.len(), CARD_ROWS, "{card:?}");
            assert!(card.iter().all(|c| c.width == DOCK_WIDTH - 1));
            assert_eq!(cells(&mut st, 160, 40, &Target::NewWorkspace).len(), 1);
            assert_eq!(cells(&mut st, 160, 40, &Target::Settings).len(), 1);
        }
    }

    /// The current workspace's card is highlighted: its name row reversed, as its tab was,
    /// the rest marked at the left.
    #[test]
    fn the_current_workspace_is_highlighted() {
        let mut st = in_workspaces("client-x");
        let buf = crate::app::tests::draw(&mut st, 160, 40);
        let row = (0..40)
            .find(|&y| {
                (0..DOCK_WIDTH)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("client-x")
            })
            .unwrap();
        let x = (0..DOCK_WIDTH)
            .find(|&x| buf[(x, row)].symbol() == "c")
            .unwrap();
        assert!(buf[(x, row)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(0, row + 1)].symbol(), "▌");
        let other = (0..40)
            .find(|&y| {
                (0..DOCK_WIDTH)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("scratch")
            })
            .unwrap();
        let x = (0..DOCK_WIDTH)
            .find(|&x| buf[(x, other)].symbol() == "s")
            .unwrap();
        assert!(!buf[(x, other)].modifier.contains(Modifier::REVERSED));
    }

    /// Narrower than 120 columns the sidebar does not dock: the tab strip is drawn, with its
    /// `≡`, and Ctrl-T, `/spaces` or a tap on `≡` open the list as a drawer, whose cards and
    /// buttons are tapped as the docked ones; Esc or a click outside it closes it.
    #[test]
    fn a_narrower_screen_opens_the_workspaces_in_a_drawer() {
        let mut st = in_workspaces("default");
        let r = rows(&mut st, 110, 30);
        assert!(!st.ws_docked() && st.tabs_shown(), "{}", r.join("\n"));
        assert!(r[0].starts_with(" ≡  default"), "{}", r[0]);
        // The ≡ opens the drawer.
        assert!(tap(&mut st, 110, 30, &Target::WsSidebar).is_empty());
        assert!(st.ws_drawer_open());
        let r = rows(&mut st, 110, 30);
        assert!(
            r[1].contains("workspaces") && r[1].contains('✕'),
            "{}",
            r.join("\n")
        );
        assert!(!st.view.ws_sidebar_hidden, "the drawer is no hiding");
        // ↓ selects the next card, Enter switches to it, and the drawer closes.
        st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Down));
        assert_eq!(st.ws_selected(), Some(WsRow::Workspace("client-x".into())));
        let a = st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Enter));
        assert_eq!(workspace_action(&a), Some(("client-x".into(), false)));
        assert!(!st.ws_drawer_open());
        // Ctrl-T opens it again, Esc closes it.
        st.switching = None;
        st.handle_key(crate::app::tests::ctrl('t'));
        assert!(st.ws_drawer_open());
        st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Esc));
        assert!(!st.ws_drawer_open());
        // A click outside it closes it and does nothing else.
        st.handle_key(crate::app::tests::ctrl('t'));
        rows(&mut st, 110, 30);
        assert!(tap_at(&mut st, 110, 30, 80, 15).is_empty());
        assert!(!st.ws_drawer_open());
        // A key it does not take closes it and goes on: typing is not lost.
        st.handle_key(crate::app::tests::ctrl('t'));
        st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Char('h')));
        assert!(!st.ws_drawer_open());
        assert_eq!(st.view.input.as_str(), "h");
    }

    /// In the compact layout the workspaces are a fourth screen, `Workspaces` (`Spaces` on a
    /// phone, with what waits in the others); the tab strip is gone. ↑↓ on an empty line
    /// select a row, Enter switches; a tap on a card switches too.
    #[test]
    fn the_compact_layout_has_a_workspaces_screen() {
        let mut st = in_workspaces("default");
        let r = rows(&mut st, 40, 20);
        assert!(st.compact() && !st.tabs_shown());
        assert!(r[0].contains("Spaces ⚠2"), "{}", r.join("\n"));
        assert!(!r.join("\n").contains("≡"));
        // Its tab, its whole share of the bar.
        let tab = cells(
            &mut st,
            40,
            20,
            &Target::Screen(crate::app::CompactPane::Workspaces),
        );
        assert_eq!(tab.len(), 1);
        assert_eq!((tab[0].x, tab[0].width), (30, 10));
        tap(
            &mut st,
            40,
            20,
            &Target::Screen(crate::app::CompactPane::Workspaces),
        );
        assert!(st.ws_screen());
        let r = rows(&mut st, 40, 20).join("\n");
        assert!(
            r.contains("client-x") && r.contains("+ New workspace"),
            "{r}"
        );
        st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Down));
        st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Down));
        assert_eq!(st.ws_selected(), Some(WsRow::Workspace("scratch".into())));
        let a = st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Enter));
        assert_eq!(workspace_action(&a), Some(("scratch".into(), false)));
        // A tap on a card switches.
        st.switching = None;
        let a = tap(&mut st, 40, 20, &Target::Workspace("client-x".into()));
        assert_eq!(workspace_action(&a), Some(("client-x".into(), false)));
        // Ctrl-T and /w show it too, and Ctrl-T on it goes back to the conversation.
        let mut st = in_workspaces("default");
        rows(&mut st, 40, 20);
        st.handle_key(crate::app::tests::ctrl('t'));
        assert!(st.ws_screen());
        st.handle_key(crate::app::tests::ctrl('t'));
        assert_eq!(st.view.compact_pane, crate::app::CompactPane::Chat);
        st.view.input.set("/w");
        st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Enter));
        assert!(st.ws_screen());
    }

    /// A tap on a card switches to that workspace, as its tab does; on the current one it
    /// says so and switches nothing.
    #[test]
    fn a_tap_on_a_card_switches() {
        let mut st = in_workspaces("default");
        let a = tap(&mut st, 160, 40, &Target::Workspace("scratch".into()));
        assert_eq!(workspace_action(&a), Some(("scratch".into(), false)));
        assert_eq!(st.switching.as_deref(), Some("scratch"));
        let mut st = in_workspaces("default");
        let a = tap(&mut st, 160, 40, &Target::Workspace("default".into()));
        assert!(workspace_action(&a).is_none());
        assert_eq!(
            st.view.status.as_deref(),
            Some("already in workspace default")
        );
        // Alt-2 switches whatever shows the workspaces, the docked sidebar too.
        let mut st = in_workspaces("default");
        rows(&mut st, 160, 40);
        let alt = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('2'),
            crossterm::event::KeyModifiers::ALT,
        );
        assert_eq!(
            workspace_action(&st.handle_key(alt)),
            Some(("client-x".into(), false))
        );
    }

    /// Ctrl-T hides the docked sidebar (the IO loop remembers it: `Action::WsSidebar`), the
    /// tab strip and its `≡` come back, and a tap on `≡` or the sidebar's `‹` toggles it
    /// as Ctrl-T does; `/spaces on|off` sets it.
    #[test]
    fn hiding_the_sidebar_brings_the_strip_back_and_is_remembered() {
        use crate::app::Action;
        let mut st = in_workspaces("default");
        rows(&mut st, 160, 40);
        let a = st.handle_key(crate::app::tests::ctrl('t'));
        assert_eq!(a, vec![Action::WsSidebar(false)]);
        assert!(st.view.ws_sidebar_hidden && !st.ws_docked() && st.tabs_shown());
        let r = rows(&mut st, 160, 40);
        assert!(r[0].starts_with(" ≡  default"), "{}", r[0]);
        let a = tap(&mut st, 160, 40, &Target::WsSidebar);
        assert_eq!(a, vec![Action::WsSidebar(true)]);
        assert!(st.ws_docked());
        let a = tap(&mut st, 160, 40, &Target::WsSidebar);
        assert_eq!(a, vec![Action::WsSidebar(false)]);
        st.view.input.set("/spaces on");
        let a = st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Enter));
        assert_eq!(a, vec![Action::WsSidebar(true)]);
        assert!(st.ws_docked());
        // With one workspace, the docked sidebar still shows (its buttons), and hidden, no
        // strip is drawn: there is nothing to switch to.
        let mut st = crate::app::tests::with_orch();
        st.workspace = Some("default".into());
        st.set_tabs(vec![Tab::new("default")]);
        let r = rows(&mut st, 160, 40).join("\n");
        assert!(r.contains("+ New workspace"), "{r}");
        st.handle_key(crate::app::tests::ctrl('t'));
        let r = rows(&mut st, 160, 40).join("\n");
        assert!(!r.contains("WORKSPACES") && !r.contains('≡'), "{r}");
        // A TUI opened on a socket has none.
        let mut st = crate::app::tests::with_orch();
        let r = rows(&mut st, 160, 40).join("\n");
        assert!(!r.contains("WORKSPACES"), "{r}");
        assert!(st.handle_key(crate::app::tests::ctrl('t')).is_empty());
    }

    /// The sidebar's state is kept in `tui-state.json`, written only when it is set: another
    /// TUI's save of what it was told keeps it as it is.
    #[test]
    fn the_sidebar_state_is_kept_across_runs() {
        use crate::firstrun;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tui-state.json");
        assert_eq!(firstrun::load(&file).workspaces_sidebar, None);
        firstrun::set_workspaces_sidebar(&file, false).unwrap();
        assert_eq!(firstrun::load(&file).workspaces_sidebar, Some(false));
        // A TUI started before, which saw it shown, saves what it told: the file's stays.
        let stale = firstrun::Seen {
            suggest_notice: true,
            workspaces_sidebar: Some(true),
            ..Default::default()
        };
        firstrun::save(&file, &stale).unwrap();
        let now = firstrun::load(&file);
        assert_eq!(now.workspaces_sidebar, Some(false));
        assert!(now.suggest_notice);
        firstrun::set_workspaces_sidebar(&file, true).unwrap();
        assert_eq!(firstrun::load(&file).workspaces_sidebar, Some(true));
    }

    /// `+ New workspace` asks for a name: what `twr ws new` refuses (and `default`, and one
    /// that exists) is said under it and Enter does nothing; a good one is created and
    /// switched to. Esc cancels. `/ws new` asks too, `/ws new <name>` creates at once.
    #[test]
    fn a_new_workspace_is_named_checked_and_created() {
        use crossterm::event::KeyCode;
        let mut st = in_workspaces("default");
        assert!(tap(&mut st, 160, 40, &Target::NewWorkspace).is_empty());
        assert_eq!(st.view.new_ws.as_deref(), Some(""));
        let r = rows(&mut st, 160, 40).join("\n");
        assert!(
            r.contains("new workspace") && r.contains("Enter creates it"),
            "{r}"
        );
        let type_in = |st: &mut AppState, text: &str| {
            for c in text.chars() {
                st.handle_key(crate::app::tests::key(KeyCode::Char(c)));
            }
        };
        for (bad, why) in [
            ("Bad_Name", "invalid workspace name"),
            ("client-x", "already exists"),
            ("default", "always exists"),
            ("-x", "invalid workspace name"),
        ] {
            st.handle_key(crate::app::tests::ctrl('u'));
            type_in(&mut st, bad);
            let r = rows(&mut st, 160, 40).join("\n");
            assert!(r.contains(why), "{bad}: {r}");
            let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
            assert!(a.is_empty(), "{bad}");
            assert!(st.view.new_ws.is_some(), "{bad}: the dialog stays");
        }
        // Keys go to the dialog only: Ctrl-T, `/`, digits type nothing else.
        st.handle_key(crate::app::tests::ctrl('u'));
        st.handle_key(crate::app::tests::ctrl('t'));
        assert!(st.ws_docked());
        type_in(&mut st, "web-2");
        assert_eq!(st.view.input.as_str(), "");
        let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert_eq!(workspace_action(&a), Some(("web-2".into(), true)));
        assert!(st.view.new_ws.is_none());
        // Esc cancels; ✕ too.
        let mut st = in_workspaces("default");
        st.open_new_ws(None);
        st.handle_key(crate::app::tests::key(KeyCode::Esc));
        assert!(st.view.new_ws.is_none());
        st.open_new_ws(None);
        tap(&mut st, 160, 40, &Target::Close);
        assert!(st.view.new_ws.is_none());
        // The commands.
        st.view.input.set("/ws new");
        st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert_eq!(st.view.new_ws.as_deref(), Some(""));
        let mut st = in_workspaces("default");
        st.view.input.set("/ws new api");
        let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert_eq!(workspace_action(&a), Some(("api".into(), true)));
        let mut st = in_workspaces("default");
        st.view.input.set("/ws new Nope");
        let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert!(workspace_action(&a).is_none());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("invalid workspace name"));
    }

    /// A TUI an agent runs creates no workspace: the button, `/ws new` and `/ws <new name>`
    /// say so and ask nothing (the CLI refuses an agent's workspace as well).
    #[test]
    fn an_agent_cannot_create_a_workspace() {
        use crossterm::event::KeyCode;
        let agent = || {
            let mut st = in_workspaces("default");
            st.agent = Some("thread web's agent (pid 42)".into());
            st
        };
        let mut st = agent();
        assert!(tap(&mut st, 160, 40, &Target::NewWorkspace).is_empty());
        assert!(st.view.new_ws.is_none());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("agents cannot create workspaces"));
        for line in ["/ws new", "/ws new api", "/ws api"] {
            let mut st = agent();
            st.view.input.set(line);
            let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
            assert!(workspace_action(&a).is_none(), "{line}");
            assert!(
                st.view.new_ws.is_none() && st.view.confirm.is_none(),
                "{line}"
            );
            let status = st.view.status.clone().unwrap_or_default();
            assert!(
                status.contains("agents cannot create workspaces"),
                "{line}: {status}"
            );
            // Switching to one that exists still works.
            let a = st.switch_workspace("client-x");
            assert_eq!(workspace_action(&a), Some(("client-x".into(), false)));
        }
    }

    /// `⚙ Settings` opens the settings, asks the hub for its own, and shows each value in
    /// effect with where it is kept; Enter changes the selected one at once (the IO loop
    /// writes it), the hub's and the models are shown only, and a TUI an agent runs changes
    /// nothing.
    #[test]
    fn the_settings_show_and_change_what_users_toggle() {
        use crate::app::Action;
        use crate::settings::{Change, Key};
        use crossterm::event::KeyCode;
        use twr_core::config::ColorMode;
        let mut st = in_workspaces("default");
        st.view.suggest = true;
        let a = tap(&mut st, 160, 40, &Target::Settings);
        assert_eq!(a, vec![Action::ConfigShow, Action::SettingsFile]);
        st.apply_result(
            "config.show",
            &serde_json::json!({"codex": {"auto": "ask"}, "wake_after_secs": 45}),
        );
        let r = rows(&mut st, 160, 40).join("\n");
        for want in [
            "settings",
            "Suggestions",
            "[tui] suggest",
            "Summaries",
            "[tui] summaries",
            "Summary pace",
            "[tui] summary_every",
            "Card order",
            "stable",
            "Notifications",
            "[tui.notify] terminal",
            "Compact below",
            "60 columns",
            "Workspace sidebar",
            "shown",
            "Colors",
            "[tui] colors",
            "Codex auto",
            "ask",
            "restart the hub",
            "Wake after",
            "45 s",
        ] {
            assert!(r.contains(want), "{want:?} in {r}");
        }
        // Enter on the first row turns suggestions off, at once.
        let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert_eq!(a, vec![Action::Setting(Change::Suggest(false))]);
        assert!(!st.view.suggest);
        // Summaries, as `/summaries` turns them; their pace is shown only.
        st.summary.on = true;
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert_eq!(a, vec![Action::Setting(Change::Summaries(false))]);
        assert!(!st.summary.on);
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        assert!(st
            .handle_key(crate::app::tests::key(KeyCode::Enter))
            .is_empty());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("[tui] summary_every"));
        // A tap selects a row, a tap on the selected one changes it.
        assert!(tap(&mut st, 160, 40, &Target::Setting(Key::CardOrder)).is_empty());
        let a = tap(&mut st, 160, 40, &Target::Setting(Key::CardOrder));
        assert_eq!(
            a,
            vec![Action::Setting(Change::CardOrder(
                twr_core::config::CardOrder::Attention
            ))]
        );
        // Notifications and the compact width step through their values, ← back.
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        let a = st.handle_key(crate::app::tests::key(KeyCode::Right));
        assert_eq!(
            a,
            vec![Action::Setting(Change::Notify(
                twr_core::config::TerminalAlert::Bell
            ))]
        );
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        let a = st.handle_key(crate::app::tests::key(KeyCode::Right));
        assert_eq!(a, vec![Action::Setting(Change::CompactWidth(80))]);
        let a = st.handle_key(crate::app::tests::key(KeyCode::Left));
        assert_eq!(a, vec![Action::Setting(Change::CompactWidth(60))]);
        // The sidebar, kept as Ctrl-T keeps it.
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert_eq!(a, vec![Action::WsSidebar(false)]);
        assert!(st.view.settings.is_some(), "it stays open");
        // The colors step through auto, 256, 16 and none, and apply at once.
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        for (want, depth) in [
            (ColorMode::Full, crate::color::Depth::Full),
            (ColorMode::Basic, crate::color::Depth::Basic),
            (ColorMode::None, crate::color::Depth::None),
        ] {
            let a = st.handle_key(crate::app::tests::key(KeyCode::Right));
            assert_eq!(a, vec![Action::Setting(Change::Colors(want))]);
            assert_eq!((st.view.color_mode, st.view.colors), (want, depth));
        }
        let r = rows(&mut st, 160, 40).join("\n");
        assert!(r.contains("no color"), "{r}");
        let a = st.handle_key(crate::app::tests::key(KeyCode::Left));
        assert_eq!(a, vec![Action::Setting(Change::Colors(ColorMode::Basic))]);
        // The hub's rows change nothing.
        st.handle_key(crate::app::tests::key(KeyCode::Down));
        assert!(st
            .handle_key(crate::app::tests::key(KeyCode::Enter))
            .is_empty());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("restart the hub"));
        // Esc closes.
        st.handle_key(crate::app::tests::key(KeyCode::Esc));
        assert!(st.view.settings.is_none());
        // `/settings` opens them too; an agent's TUI shows them read-only.
        let mut st = in_workspaces("default");
        st.agent = Some("thread web's agent".into());
        st.view.input.set("/settings");
        assert_eq!(
            st.handle_key(crate::app::tests::key(KeyCode::Enter)),
            vec![Action::ConfigShow, Action::SettingsFile]
        );
        let r = rows(&mut st, 160, 40).join("\n");
        assert!(r.contains("read-only"), "{r}");
        assert!(st
            .handle_key(crate::app::tests::key(KeyCode::Enter))
            .is_empty());
        assert!(st.view.suggest || !st.view.suggest);
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("changes no setting"));
    }

    /// The settings fit a phone: the selected row stays on screen as ↑↓ move.
    #[test]
    fn the_settings_scroll_on_a_phone() {
        use crossterm::event::KeyCode;
        let mut st = in_workspaces("default");
        rows(&mut st, 40, 20);
        st.open_settings();
        for _ in 0..crate::settings::KEYS.len() {
            st.handle_key(crate::app::tests::key(KeyCode::Down));
        }
        let r = rows(&mut st, 40, 20).join("\n");
        assert!(r.contains("Wake after"), "{r}");
        assert!(!cells(
            &mut st,
            40,
            20,
            &Target::Setting(crate::settings::Key::WakeAfter)
        )
        .is_empty());
    }

    /// The views get the columns the docked sidebar leaves: what depends on their width (a
    /// dive hiding the cards below 100 columns, the grid's columns) counts those.
    #[test]
    fn the_views_count_the_columns_the_dock_leaves() {
        let mut st = in_workspaces("default");
        rows(&mut st, 122, 30);
        assert!(st.ws_docked());
        assert_eq!(st.content_width(), 122 - DOCK_WIDTH);
        st.dive("t-gamma");
        assert!(st.dive_hides_cards(), "96 columns are left for the stack");
        st.view.ws_sidebar_hidden = true;
        assert!(!st.dive_hides_cards());
    }

    /// A paste goes into the new-workspace dialog (its first line), never into the message
    /// being typed under it; with the settings open it goes nowhere.
    #[test]
    fn a_paste_goes_to_the_dialog() {
        let mut st = in_workspaces("default");
        st.view.input.set("draft");
        st.open_new_ws(None);
        st.handle_paste("web-3\nsecond line");
        assert_eq!(st.view.new_ws.as_deref(), Some("web-3"));
        assert_eq!(st.view.input.as_str(), "draft");
        st.view.new_ws = None;
        st.open_settings();
        st.handle_paste("x");
        assert_eq!(st.view.input.as_str(), "draft");
    }

    /// A conversation kept from before (the main pane shows a dive now) gets no more events:
    /// the card shows the hub's reply, not the kept one's.
    #[test]
    fn a_kept_conversation_does_not_shadow_newer_replies() {
        let mut st = in_workspaces("default");
        let orch = st.orchestrator().unwrap().id.clone();
        let mut t = crate::app::Transcript::new(orch);
        t.settle();
        t.entries.push(crate::app::Entry::Text {
            seq: 1,
            text: "old reply".into(),
            open: false,
        });
        st.transcripts.push(t);
        st.tabs[0].glance = Some(Glance {
            active: false,
            orch: Some(OrchGlance {
                status: ThreadStatus::Idle,
                preview: Some("new reply".into()),
                ..Default::default()
            }),
        });
        let g = st.tab_glance(&st.tabs[0].clone()).unwrap();
        assert_eq!(g.orch.unwrap().preview.as_deref(), Some("new reply"));
    }

    /// The wheel scrolls the list away from the selected card, and it stays there until a
    /// key moves the selection.
    #[test]
    fn the_wheel_scrolls_past_the_selection() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let mut st = in_workspaces("default");
        let mut tabs = st.tabs.clone();
        for i in 0..8 {
            tabs.push(Tab::new(format!("more-{i}")));
        }
        st.set_tabs(tabs);
        rows(&mut st, 40, 20);
        st.handle_key(crate::app::tests::ctrl('t'));
        let first = rows(&mut st, 40, 20).join("\n");
        assert!(
            first.contains("default") && !first.contains("more-7"),
            "{first}"
        );
        let list = st.view.panes.get(&PaneId::Workspaces).unwrap().area;
        let screen = crate::app::tests::draw(&mut st, 40, 20);
        for _ in 0..60 {
            st.handle_mouse(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: list.x + 1,
                    row: list.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
                &screen,
            );
            rows(&mut st, 40, 20);
        }
        let end = rows(&mut st, 40, 20).join("\n");
        assert!(end.contains("more-7") && !end.contains("▌"), "{end}");
        // A key brings the selection back into view.
        st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Down));
        let back = rows(&mut st, 40, 20).join("\n");
        assert!(back.contains("client-x"), "{back}");
    }

    /// The first-run picker (or the help) over the view leaves the docked sidebar on screen:
    /// a tap on a workspace's card switches, as a tab did; its buttons wait.
    #[test]
    fn a_card_switches_from_under_the_picker_and_the_help() {
        let picker = || crate::app::Picker {
            runtimes: vec![("claude-code".into(), Some(true))],
            selected: 0,
            default: "claude-code".into(),
            model: None,
            switch: false,
        };
        let mut st = in_workspaces("default");
        st.view.picker = Some(picker());
        assert!(st.picker_open());
        let a = tap(&mut st, 160, 40, &Target::Workspace("client-x".into()));
        assert_eq!(workspace_action(&a), Some(("client-x".into(), false)));
        let mut st = in_workspaces("default");
        st.view.picker = Some(picker());
        assert!(tap(&mut st, 160, 40, &Target::Settings).is_empty());
        assert!(st.view.settings.is_none());
        let mut st = in_workspaces("default");
        st.view.help = true;
        let a = tap(&mut st, 160, 40, &Target::Workspace("scratch".into()));
        assert_eq!(workspace_action(&a), Some(("scratch".into(), false)));
        assert!(!st.view.help);
    }

    /// With the settings or the new-workspace dialog open, a tap on a tab of the strip
    /// switches, but a press in the dialog released on a tab does not.
    #[test]
    fn a_drag_onto_a_tab_from_a_dialog_switches_nothing() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut st = in_workspaces("default");
        st.open_settings();
        rows(&mut st, 110, 30);
        let tab = st
            .view
            .panes
            .panes
            .iter()
            .find(|p| p.id == PaneId::Tab("scratch".into()))
            .unwrap()
            .area;
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let screen = crate::app::tests::draw(&mut st, 110, 30);
        st.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left), 55, 15), &screen);
        let a = st.handle_mouse(
            ev(MouseEventKind::Up(MouseButton::Left), tab.x + 1, tab.y),
            &screen,
        );
        assert!(workspace_action(&a).is_none());
        let a = tap_at(&mut st, 110, 30, tab.x + 1, tab.y);
        assert_eq!(workspace_action(&a), Some(("scratch".into(), false)));
    }

    /// Under the settings, a tab that moved under the finger between the press and the
    /// release (a redraw in between) is not switched to: only the tab pressed is.
    #[test]
    fn a_tab_moved_under_the_finger_is_not_switched_to() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut st = in_workspaces("default");
        st.open_settings();
        rows(&mut st, 110, 30);
        let tab = st
            .view
            .panes
            .panes
            .iter()
            .find(|p| p.id == PaneId::Tab("scratch".into()))
            .unwrap()
            .area;
        let ev = |kind| MouseEvent {
            kind,
            column: tab.x + 1,
            row: tab.y,
            modifiers: KeyModifiers::NONE,
        };
        let screen = crate::app::tests::draw(&mut st, 110, 30);
        st.handle_mouse(ev(MouseEventKind::Down(MouseButton::Left)), &screen);
        // `scratch` is gone: `docs` is drawn where it was.
        let tabs: Vec<Tab> = st
            .tabs
            .iter()
            .filter(|t| t.name != "scratch")
            .cloned()
            .collect();
        st.set_tabs(tabs);
        let screen = crate::app::tests::draw(&mut st, 110, 30);
        assert_eq!(
            st.view.panes.at(tab.x + 1, tab.y).map(|p| p.id.clone()),
            Some(PaneId::Tab("docs".into()))
        );
        let a = st.handle_mouse(ev(MouseEventKind::Up(MouseButton::Left)), &screen);
        assert!(workspace_action(&a).is_none());
    }

    /// With the help or the trash open, the wheel over the docked workspace list scrolls the
    /// list, not the overlay.
    #[test]
    fn the_wheel_scrolls_the_dock_beside_the_help() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let mut st = in_workspaces("default");
        let mut tabs = st.tabs.clone();
        for i in 0..8 {
            tabs.push(Tab::new(format!("more-{i}")));
        }
        st.set_tabs(tabs);
        st.view.help = true;
        rows(&mut st, 160, 30);
        let screen = crate::app::tests::draw(&mut st, 160, 30);
        st.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 2,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &screen,
        );
        assert_eq!((st.view.ws_scroll, st.view.help_scroll), (1, 0));
        assert!(st.view.help);
    }

    /// On a phone the new workspace's name keeps its end and the cursor in view, however
    /// long it gets.
    #[test]
    fn a_long_name_keeps_its_end_in_view() {
        use crossterm::event::KeyCode;
        let mut st = in_workspaces("default");
        st.open_new_ws(None);
        let name = "abcdefghijklmnopqrstuvwxyz012345";
        assert_eq!(name.len(), twr_core::paths::MAX_WORKSPACE_NAME);
        for c in name.chars() {
            st.handle_key(crate::app::tests::key(KeyCode::Char(c)));
        }
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 20)).unwrap();
        (st.view.width, st.view.height) = (40, 20);
        term.draw(|f| {
            crate::ui::render(f, &st);
        })
        .unwrap();
        let r = rows(&mut st, 40, 20).join("\n");
        assert!(r.contains("012345"), "{r}");
        let cursor = term.get_cursor_position().unwrap();
        assert!(cursor.x < 39, "{cursor:?}");
        let a = st.handle_key(crate::app::tests::key(KeyCode::Enter));
        assert_eq!(workspace_action(&a), Some((name.into(), true)));
    }

    /// The current workspace's hub gone (a poll found nothing listening): its card says so,
    /// with the last reply, rather than what the view last knew.
    #[test]
    fn a_stopped_current_hub_shows_as_stopped() {
        let mut st = in_workspaces("default");
        let mut tabs = st.tabs.clone();
        tabs[0].hub = TabHub::Stopped;
        tabs[0].glance = Some(Glance {
            active: false,
            orch: Some(OrchGlance {
                status: ThreadStatus::Idle,
                preview: Some("last words".into()),
                ..Default::default()
            }),
        });
        st.set_tabs(tabs);
        let tab = st.tabs[0].clone();
        assert_eq!(st.tab_hub(&tab), TabHub::Stopped);
        assert!(!st.tab_glance(&tab).unwrap().active);
        let r = rows(&mut st, 160, 40).join("\n");
        assert!(r.contains("○ default") && r.contains("last words"), "{r}");
    }

    /// The control stream lost (the hub stopped or restarted under the view): the current
    /// card goes by the polls, keeping the newest reply the conversation had while no hub
    /// answers; a stopped card's icon does not move, whatever its last glance said.
    #[test]
    fn a_lost_current_hub_goes_by_the_polls() {
        let mut st = in_workspaces("default");
        let orch = st.orchestrator().unwrap().id.clone();
        let mut t = crate::app::Transcript::new(orch.clone());
        t.settle();
        let mut o = twr_core::testkit::ThreadScript::new(orch);
        t.apply(&o.turn_started("hi"));
        t.apply(&o.text("reply B"));
        st.transcript = Some(t);
        let mut tabs = st.tabs.clone();
        tabs[0].hub = TabHub::Busy;
        tabs[0].glance = Some(Glance {
            active: true,
            orch: Some(OrchGlance {
                status: ThreadStatus::Running,
                preview: Some("reply A".into()),
                ..Default::default()
            }),
        });
        st.set_tabs(tabs);
        st.control_ended();
        let tab = st.tabs[0].clone();
        assert_eq!(st.tab_hub(&tab), TabHub::Busy);
        let g = st.tab_glance(&tab).unwrap();
        assert!(!g.active);
        assert_eq!(g.orch.unwrap().preview.as_deref(), Some("reply B"));
        // Restarted: the hub answers again, and says what is current.
        let mut tabs = st.tabs.clone();
        tabs[0].hub = TabHub::Running {
            threads: 0,
            running: 0,
            permissions: 0,
            questions: 0,
        };
        tabs[0].glance = Some(Glance {
            active: false,
            orch: Some(OrchGlance {
                status: ThreadStatus::Idle,
                preview: Some("reply C".into()),
                ..Default::default()
            }),
        });
        st.set_tabs(tabs);
        let g = st.tab_glance(&st.tabs[0].clone()).unwrap();
        assert_eq!(g.orch.unwrap().preview.as_deref(), Some("reply C"));
        // A stopped card whose last glance was active keeps no clock ticking.
        let mut st = in_workspaces("default");
        let mut tabs = st.tabs.clone();
        tabs[1].hub = TabHub::Stopped;
        st.set_tabs(tabs);
        assert!(st.tabs[1].glance.as_ref().unwrap().active);
        st.view.animate = true;
        rows(&mut st, 160, 40);
        let dock = Rect::new(0, 0, DOCK_WIDTH, 40);
        assert!(
            st.view
                .panes
                .moving
                .iter()
                .all(|c| !c.intersects(dock) || c.y < 5),
            "{:?}",
            st.view.panes.moving
        );
    }

    /// Of the conversation's reply and the hub's, the card shows the later one, by their
    /// seqs: a reply the hub had after the conversation stopped following (its subscription
    /// failed, or the hub restarted and was stopped again) is not hidden by an older one.
    #[test]
    fn the_newest_reply_wins() {
        let mut st = in_workspaces("default");
        let orch = st.orchestrator().unwrap().id.clone();
        let mut t = crate::app::Transcript::new(orch.clone());
        t.settle();
        let mut o = twr_core::testkit::ThreadScript::new(orch.clone());
        t.apply(&o.turn_started("hi"));
        t.apply(&o.text("reply B"));
        let b = t.last_reply.as_ref().unwrap().0;
        st.transcript = Some(t);
        let polled = |seq: u64, text: &str| Glance {
            active: false,
            orch: Some(OrchGlance {
                id: Some(orch.clone()),
                preview_seq: Some(seq),
                status: ThreadStatus::Idle,
                preview: Some(text.into()),
            }),
        };
        let preview = |st: &AppState| {
            st.tab_glance(&st.tabs[0].clone())
                .unwrap()
                .orch
                .unwrap()
                .preview
                .unwrap()
        };
        // Live: the hub's later reply wins, its earlier one does not.
        st.tabs[0].glance = Some(polled(b + 5, "reply C"));
        assert_eq!(preview(&st), "reply C");
        st.tabs[0].glance = Some(polled(b - 1, "reply A"));
        assert_eq!(preview(&st), "reply B");
        // Lost, then stopped: the later of the two still.
        st.control_ended();
        st.tabs[0].hub = TabHub::Stopped;
        st.tabs[0].glance = Some(polled(b + 5, "reply C"));
        assert_eq!(preview(&st), "reply C");
        st.tabs[0].glance = Some(polled(b - 1, "reply A"));
        assert_eq!(preview(&st), "reply B");
    }

    /// A reply the conversation showed after the last poll stays on the card once the view is
    /// left, and when that hub then stops before a poll caught up.
    #[test]
    fn a_live_reply_stays_on_the_card_after_a_switch() {
        let mut st = in_workspaces("default");
        let orch = st.orchestrator().unwrap().id.clone();
        let mut t = crate::app::Transcript::new(orch.clone());
        t.settle();
        let mut o = twr_core::testkit::ThreadScript::new(orch.clone());
        t.apply(&o.turn_started("hi"));
        t.apply(&o.text("reply B"));
        let b = t.last_reply.as_ref().unwrap().0;
        st.transcript = Some(t);
        st.tabs[0].glance = Some(Glance {
            active: true,
            orch: Some(OrchGlance {
                id: Some(orch.clone()),
                preview_seq: Some(b - 1),
                status: ThreadStatus::Running,
                preview: Some("reply A".into()),
            }),
        });
        st.keep_live_glance();
        // In the next view (another workspace), its card is the tab as kept.
        let mut next = in_workspaces("client-x");
        next.set_tabs(st.tabs.clone());
        next.tabs[0].hub = TabHub::Stopped;
        let g = next.tab_glance(&next.tabs[0].clone()).unwrap();
        let o = g.orch.unwrap();
        assert_eq!(
            (o.preview.as_deref(), o.preview_seq),
            (Some("reply B"), Some(b))
        );
        // Its hub stopped: nothing shows working, whatever was kept.
        assert!(!crate::sidebar::tab_active(&next, "default"));
    }

    /// A dive moves the orchestrator's conversation aside: its reply, newer than the last
    /// poll's, stays on the card, and once that hub stops.
    #[test]
    fn a_live_reply_stays_on_the_card_during_a_dive() {
        let mut st = in_workspaces("default");
        let orch = st.orchestrator().unwrap().id.clone();
        let mut t = crate::app::Transcript::new(orch.clone());
        t.settle();
        let mut o = twr_core::testkit::ThreadScript::new(orch.clone());
        t.apply(&o.turn_started("hi"));
        t.apply(&o.text("reply B"));
        let b = t.last_reply.as_ref().unwrap().0;
        st.transcripts.push(t);
        st.tabs[0].glance = Some(Glance {
            active: false,
            orch: Some(OrchGlance {
                id: Some(orch),
                preview_seq: Some(b - 1),
                status: ThreadStatus::Idle,
                preview: Some("reply A".into()),
            }),
        });
        let preview = |st: &AppState| {
            st.tab_glance(&st.tabs[0].clone())
                .unwrap()
                .orch
                .unwrap()
                .preview
                .unwrap()
        };
        assert_eq!(preview(&st), "reply B");
        st.control_ended();
        st.tabs[0].hub = TabHub::Stopped;
        assert_eq!(preview(&st), "reply B");
    }

    /// The orchestrator's newest reply stays on the card after its transcript left the cache
    /// of conversations (many dives since), and once its hub stopped.
    #[test]
    fn a_live_reply_outlives_its_transcript() {
        use twr_core::proto::{StreamItem, StreamKind};
        let mut st = in_workspaces("default");
        let orch = st.orchestrator().unwrap().id.clone();
        st.transcript = Some({
            let mut t = crate::app::Transcript::new(orch.clone());
            t.settle();
            t
        });
        let mut o = twr_core::testkit::ThreadScript::new(orch.clone());
        o.at(1000);
        for ev in [o.turn_started("hi"), o.text("reply B")] {
            st.apply_event(&StreamItem {
                stream: StreamKind::Thread,
                gseq: None,
                event: ev,
            });
        }
        let b = st.orch_reply.as_ref().unwrap().1;
        st.transcript = None;
        st.transcripts.clear();
        st.tabs[0].glance = Some(Glance {
            active: false,
            orch: Some(OrchGlance {
                id: Some(orch),
                preview_seq: Some(b - 1),
                status: ThreadStatus::Idle,
                preview: Some("reply A".into()),
            }),
        });
        st.control_ended();
        st.tabs[0].hub = TabHub::Stopped;
        let g = st.tab_glance(&st.tabs[0].clone()).unwrap();
        assert_eq!(g.orch.unwrap().preview.as_deref(), Some("reply B"));
    }

    /// From the compact layout's decisions overlay, Ctrl-T shows the Workspaces screen.
    #[test]
    fn ctrl_t_leaves_the_compact_decisions_for_the_workspaces() {
        let mut st = in_workspaces("default");
        rows(&mut st, 40, 20);
        st.toggle_decisions();
        st.handle_key(crate::app::tests::ctrl('t'));
        assert!(st.ws_screen());
        let r = rows(&mut st, 40, 20).join("\n");
        assert!(r.contains("+ New workspace"), "{r}");
    }

    /// A drawer opened on a narrower screen does not come back once the docked sidebar,
    /// after a resize, is hidden.
    #[test]
    fn a_drawer_does_not_come_back_with_the_dock_hidden() {
        let mut st = in_workspaces("default");
        rows(&mut st, 110, 30);
        st.handle_key(crate::app::tests::ctrl('t'));
        assert!(st.ws_drawer_open());
        rows(&mut st, 160, 40);
        assert!(st.ws_docked() && !st.ws_drawer_open());
        st.handle_key(crate::app::tests::ctrl('t'));
        rows(&mut st, 110, 30);
        assert!(!st.ws_drawer_open());
    }

    /// Under the settings, the wheel over the docked workspace list scrolls it, and changes
    /// no setting's selection.
    #[test]
    fn the_wheel_scrolls_the_dock_under_the_settings() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let mut st = in_workspaces("default");
        let mut tabs = st.tabs.clone();
        for i in 0..8 {
            tabs.push(Tab::new(format!("more-{i}")));
        }
        st.set_tabs(tabs);
        st.open_settings();
        rows(&mut st, 160, 30);
        let screen = crate::app::tests::draw(&mut st, 160, 30);
        st.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 2,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &screen,
        );
        assert_eq!(st.view.ws_scroll, 1);
        assert_eq!(st.view.settings.as_ref().unwrap().selected, 0);
    }

    /// The sidebar, its drawer, the new-workspace dialog and the settings come down to the
    /// colors the terminal has (`[tui] colors`) with the rest of the screen: none left at
    /// `none`, the selected card and the current workspace still told apart (reverse video);
    /// only the 16 named colors at `16`.
    #[test]
    fn the_sidebar_and_its_dialogs_follow_the_colors() {
        use ratatui::buffer::Buffer;
        let named = |c: Color| !matches!(c, Color::Indexed(_) | Color::Rgb(..));
        let check = |st: &mut AppState, w: u16, h: u16, case: &str| {
            st.view.colors = crate::color::Depth::None;
            let buf: Buffer = crate::app::tests::draw(st, w, h);
            assert!(
                buf.content
                    .iter()
                    .all(|c| c.fg == Color::Reset && c.bg == Color::Reset),
                "{case}: a color at none"
            );
            assert!(
                buf.content
                    .iter()
                    .any(|c| c.modifier.contains(Modifier::REVERSED)),
                "{case}: nothing reversed"
            );
            st.view.colors = crate::color::Depth::Basic;
            let buf: Buffer = crate::app::tests::draw(st, w, h);
            assert!(
                buf.content.iter().all(|c| named(c.fg) && named(c.bg)),
                "{case}: a color past the 16 at 16"
            );
        };
        // Docked, with the settings over the view, then the new-workspace dialog.
        let mut st = in_workspaces("default");
        st.open_settings();
        check(&mut st, 160, 40, "docked, settings");
        st.view.settings = None;
        st.open_new_ws(None);
        check(&mut st, 160, 40, "docked, new workspace");
        // The drawer, a card selected with the keys.
        let mut st = in_workspaces("default");
        st.view.ws_drawer = true;
        st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Down));
        check(&mut st, 100, 30, "drawer");
        // The compact Workspaces screen.
        let mut st = in_workspaces("default");
        st.show_pane(crate::app::CompactPane::Workspaces);
        check(&mut st, 40, 20, "compact");
    }

    /// A reply a stop cut short is not the orchestrator's latest reply: the card keeps the
    /// last final one.
    #[test]
    fn a_reply_cut_short_is_not_the_latest() {
        use twr_core::event::{AgentText, EventBody};
        let mut o = twr_core::testkit::ThreadScript::new("o");
        let mut t = crate::app::Transcript::new("o");
        t.settle();
        t.apply(&o.turn_started("hi"));
        t.apply(&o.text("the whole answer"));
        t.apply(&o.turn_started("more"));
        t.apply(&o.event(EventBody::AgentText(AgentText {
            text: "half an ans".into(),
            is_final: false,
            ..Default::default()
        })));
        t.apply(&o.stopped());
        assert_eq!(
            t.last_reply.map(|(_, r)| r).as_deref(),
            Some("the whole answer")
        );
    }

    /// A reply runs together on its rows, without its Markdown marks or thread tags, cut
    /// with `…` when there is more.
    #[test]
    fn a_preview_is_its_prose_cut_to_its_rows() {
        let text = "## Done\n\n<thread name=\"api\">\n- merged **api** into `dev`\n</thread>\nAll tests pass and the branch is pushed for review.";
        assert_eq!(
            preview_rows(text, 20, 2),
            vec!["Done merged api into", "dev All tests pass…"]
        );
        assert_eq!(preview_rows("short", 20, 2), vec!["short"]);
        assert_eq!(preview_rows("", 20, 2), Vec::<String>::new());
        // A word wider than the row is cut.
        assert_eq!(preview_rows("abcdefghijklmnop", 6, 1), vec!["abcde…"]);
    }
}
