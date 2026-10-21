//! The workspace sidebar: a card per workspace (`default` and every `twr ws` workspace) with
//! its orchestrator's status and the start of its latest reply ([`crate::workspace::Glance`]),
//! whether anything there works, and the counts the tab strip shows; then `+ New workspace`
//! and `⚙ Settings`. The current workspace's card is highlighted; a tap on a card switches to
//! that workspace, as its tab does.
//!
//! Where it goes ([`AppState::ws_docked`]): a column left of the views (stack, grid and the
//! decision table) in the full layout from [`DOCK_MIN_WIDTH`] columns, unless hidden (Ctrl-T,
//! `/spaces`, its header's `‹`, kept across runs). Narrower, the same list opens as a drawer
//! over the left of the view (Ctrl-T, `/spaces`, the tab strip's `☰`), and in the compact
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
    // The card the keys select, else the current one when nothing was scrolled, stays in view.
    let keep = selected
        .clone()
        .filter(|r| matches!(r, WsRow::Workspace(_)))
        .or_else(|| {
            (state.view.ws_scroll == 0)
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

/// Whether workspace `name`'s card shows that something there works.
fn tab_active(state: &AppState, name: &str) -> bool {
    state
        .tabs
        .iter()
        .find(|t| t.name == name)
        .and_then(|t| state.tab_glance(t))
        .is_some_and(|g| g.active)
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
    let words: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| {
            !(l.starts_with("<thread") || l.starts_with("</thread") || l.starts_with("```"))
        })
        .map(|l| l.trim_start_matches(['#', '>', '-', '*', '+', ' ']))
        .flat_map(str::split_whitespace)
        .map(|w| w.replace("**", "").replace('`', ""))
        .filter(|w| !w.is_empty())
        .collect();
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut cut = false;
    for (i, word) in words.iter().enumerate() {
        let w = crate::edit::width(word);
        let used = crate::edit::width(&row);
        if row.is_empty() {
            row = crate::ui::truncate_cells(word, width);
            if w > width && rows.len() + 1 == max {
                cut = i + 1 < words.len() || w > width;
                break;
            }
        } else if used + 1 + w <= width {
            row.push(' ');
            row.push_str(word);
        } else {
            rows.push(std::mem::take(&mut row));
            if rows.len() == max {
                cut = true;
                break;
            }
            row = crate::ui::truncate_cells(word, width);
        }
    }
    if !row.is_empty() && rows.len() < max {
        rows.push(row);
    }
    if cut {
        if let Some(last) = rows.last_mut() {
            let keep = width.saturating_sub(1);
            let mut s = crate::ui::truncate_cells(last, keep);
            // (`truncate` ends what it cut with `…` itself.)
            if !s.ends_with('…') {
                s.push('…');
            }
            *last = s;
        }
    }
    rows
}

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
    let mut lines = vec![Line::from(vec![
        Span::raw(" name: "),
        Span::styled(
            name.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
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
    let x = inner.x + 7 + u16::try_from(crate::edit::width(name)).unwrap_or(u16::MAX);
    if inner.height > 0 && x < inner.x + inner.width {
        frame.set_cursor_position((x, inner.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply runs together on its rows, without its Markdown marks or thread tags, cut
    /// with `…` when there is more.
    #[test]
    fn a_preview_is_its_prose_cut_to_its_rows() {
        let text = "## Done\n\n<thread name=\"api\">\n- merged **api** into `dev`\n</thread>\nAll tests pass and the branch is pushed for review.";
        assert_eq!(
            preview_rows(text, 20, 2),
            vec!["Done merged api into", "dev All tests pass …"]
        );
        assert_eq!(preview_rows("short", 20, 2), vec!["short"]);
        assert_eq!(preview_rows("", 20, 2), Vec::<String>::new());
        // A word wider than the row is cut.
        assert_eq!(preview_rows("abcdefghijklmnop", 6, 1), vec!["abcde…"]);
    }
}
