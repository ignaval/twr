//! The workspace menu: what can be done to one workspace ([`twr_core::workspaces`]), opened
//! from the `⋯` on its card in the Threads sidebar (or the compact Threads screen), or with
//! `/ws menu [name]`, `/ws rename` and `/ws rm`. Its choices: Open, Rename…, Make default,
//! Delete…; Rename asks for the new name as the new-workspace dialog does, and Delete shows
//! what deleting would remove and keep (the CLI finds that out,
//! [`crate::Workspaces::plan_removal`]) under a `Delete <name>` and a `Cancel` that stay on
//! screen however long that is: the text scrolls above them. Delete runs on `y`, or Enter on
//! it, or a second tap on it, once the plan has been on screen [`CARD_SETTLE`]; it opens on
//! Cancel, so Enter alone never deletes.
//!
//! Only the user changes workspaces: a TUI that an agent runs opens no menu. Deleting the
//! workspace the TUI shows switches to another one first (the default, else the next); the
//! only workspace there is can't be deleted here (`twr ws rm` can, and `twr` then asks for a
//! new one).

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{Action, AppState, CARD_SETTLE};
use crate::select::{PaneId, Panes, Target};
use crate::RemovalPlan;

/// Widest the menu is drawn.
const MAX_WIDTH: u16 = 64;

/// Longest name the rename field takes (a name is 32 at most; more shows why it can't be).
const NAME_MAX: usize = 40;

/// What the menu is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Its choices.
    Choices,
    /// Asking for the new name: what was typed.
    Rename(String),
    /// Finding out what deleting it would do.
    Checking,
    /// What deleting it would do, asking to confirm (or why it can't be deleted now).
    Plan(RemovalPlan),
}

/// A choice of the menu, as tapped ([`Target::WsChoice`]) or run with Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Choice {
    Open,
    Rename,
    MakeDefault,
    Delete,
    /// The rename's Enter.
    SaveName,
    /// Delete, confirmed.
    ConfirmDelete,
    /// Back out: closes the menu.
    Cancel,
}

/// The workspace menu, open on workspace `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsMenu {
    /// The workspace, by id.
    pub id: String,
    pub step: Step,
    /// The highlighted choice.
    pub sel: Choice,
    /// The first row of the plan shown (it scrolls above the choices; the most it scrolls is
    /// as last drawn, [`Panes::ws_menu_scroll`]).
    pub scroll: usize,
    /// A tap highlighted Delete: the next tap on it deletes.
    pub armed: bool,
    /// When the plan came: Delete waits [`CARD_SETTLE`] after.
    pub shown_at: Option<Instant>,
}

impl WsMenu {
    fn new(id: &str, step: Step) -> WsMenu {
        let sel = match &step {
            Step::Rename(_) => Choice::SaveName,
            Step::Choices => Choice::Rename,
            _ => Choice::Cancel,
        };
        WsMenu {
            id: id.to_string(),
            step,
            sel,
            scroll: 0,
            armed: false,
            shown_at: None,
        }
    }
}

impl AppState {
    /// Why this TUI can't change workspaces, if it can't: opened on a socket, or run for an
    /// agent (only the user changes workspaces, as the CLI refuses an agent's).
    fn ws_menu_refusal(&self) -> Option<String> {
        if self.workspace.is_none() {
            return Some("workspaces: not here (this TUI was opened on a socket)".into());
        }
        self.agent.as_ref().map(|agent| {
            format!(
                "this TUI runs for {agent}: agents cannot rename, delete or change workspaces; \
                 only you can, from your own terminal"
            )
        })
    }

    /// Opens the workspace menu on the workspace `selector` names (by name or id; `None`:
    /// the current one) at `step`, or says why it can't.
    fn open_ws_menu_at(&mut self, selector: Option<&str>, step: Step) -> Vec<Action> {
        if let Some(why) = self.ws_menu_refusal() {
            self.view.status = Some(why);
            return Vec::new();
        }
        let id = match selector {
            None => self.workspace.clone(),
            Some(s) => self.ws_resolve(s),
        };
        let Some(id) = id else {
            self.view.status = Some(format!(
                "no workspace {} (/ws lists them)",
                selector.unwrap_or_default()
            ));
            return Vec::new();
        };
        self.view.new_ws = None;
        self.view.action = None;
        self.view.confirm = None;
        let delete = step == Step::Checking;
        self.view.ws_menu = Some(WsMenu::new(&id, step));
        if delete {
            return self.ws_menu_delete();
        }
        Vec::new()
    }

    /// `⋯` on a workspace's card, `/ws menu [name]`: its menu.
    pub fn open_ws_menu(&mut self, selector: Option<&str>) -> Vec<Action> {
        self.open_ws_menu_at(selector, Step::Choices)
    }

    /// `/ws rename [name] <new>`: the menu's rename, `new` typed in.
    pub fn ws_rename_cmd(&mut self, selector: Option<&str>, to: &str) -> Vec<Action> {
        self.open_ws_menu_at(selector, Step::Rename(to.to_string()))
    }

    /// `/ws rm <name>`: the menu's Delete: what deleting would do, then Delete to do it.
    pub fn ws_rm_cmd(&mut self, selector: &str) -> Vec<Action> {
        self.open_ws_menu_at(Some(selector), Step::Checking)
    }

    /// The menu's choices as drawn, in order, each with what it says.
    pub fn ws_menu_choices(&self) -> Vec<(Choice, String)> {
        let Some(m) = &self.view.ws_menu else {
            return Vec::new();
        };
        let label = self.ws_label(&m.id);
        let tab = self.tabs.iter().find(|t| t.name == m.id);
        match &m.step {
            Step::Choices => {
                let mut out = Vec::new();
                if self.workspace.as_deref() != Some(m.id.as_str()) {
                    out.push((Choice::Open, format!("Open {label}")));
                }
                out.push((Choice::Rename, "Rename…".to_string()));
                if !tab.is_some_and(|t| t.default) {
                    out.push((Choice::MakeDefault, "Make default".to_string()));
                }
                out.push((Choice::Delete, "Delete…".to_string()));
                out
            }
            Step::Rename(_) => vec![
                (Choice::SaveName, "Rename".to_string()),
                (Choice::Cancel, "Cancel".to_string()),
            ],
            Step::Checking => vec![(Choice::Cancel, "Cancel".to_string())],
            Step::Plan(p) if p.refused.is_some() => vec![(Choice::Cancel, "Close".to_string())],
            Step::Plan(_) => vec![
                (Choice::ConfirmDelete, format!("Delete {label}")),
                (Choice::Cancel, "Cancel".to_string()),
            ],
        }
    }

    /// The text the menu shows above its choices, a line each (wrapped as drawn).
    pub fn ws_menu_text(&self) -> Vec<String> {
        let Some(m) = &self.view.ws_menu else {
            return Vec::new();
        };
        let tab = self.tabs.iter().find(|t| t.name == m.id);
        let label = self.ws_label(&m.id);
        match &m.step {
            Step::Choices => {
                let mut about = Vec::new();
                if tab.is_some_and(|t| t.default) {
                    about.push("the default workspace".to_string());
                }
                if self.workspace.as_deref() == Some(m.id.as_str()) {
                    about.push("open here".to_string());
                }
                if label != m.id {
                    about.push(format!("-w {} still selects it", m.id));
                }
                if about.is_empty() {
                    Vec::new()
                } else {
                    vec![about.join(" · ")]
                }
            }
            Step::Rename(name) => {
                let note = match self.ws_name_error(Some(&m.id), name) {
                    Some(why) if !name.is_empty() && *name != label => why,
                    _ => "a-z, 0-9 and - · nothing in it moves; its hub keeps running · Enter \
                          renames · Esc cancels"
                        .to_string(),
                };
                vec![format!("name: {name}"), String::new(), note]
            }
            Step::Checking => vec![format!("finding out what deleting {label} would remove…")],
            Step::Plan(p) => {
                let mut lines = Vec::new();
                if let Some(why) = &p.refused {
                    lines.push(format!("{label} can't be deleted now: {why}"));
                    return lines;
                }
                lines.push(format!("Delete workspace {label}? This cannot be undone."));
                if self.workspace.as_deref() == Some(m.id.as_str()) {
                    if let Some(to) = self.ws_removal_via(&m.id) {
                        lines.push(format!(
                            "You are in it: the TUI switches to {} first.",
                            self.ws_label(&to)
                        ));
                    }
                }
                if p.stops_hub {
                    lines.push(
                        "Its hub is running (no threads, its orchestrator idle): deleting stops \
                         it first."
                            .to_string(),
                    );
                }
                lines.extend(p.lines.iter().cloned());
                lines
            }
        }
    }

    /// Where the TUI goes before it deletes workspace `id`, the one it shows: the default, or
    /// else the first other one listed. `None`: there is no other.
    pub fn ws_removal_via(&self, id: &str) -> Option<String> {
        self.tabs
            .iter()
            .filter(|t| t.name != id)
            .find(|t| t.default)
            .or_else(|| self.tabs.iter().find(|t| t.name != id))
            .map(|t| t.name.clone())
    }

    /// Delete…: asks the CLI what deleting would do (the menu says it is checking until the
    /// plan comes, [`AppState::ws_planned`]), unless it is the only workspace.
    fn ws_menu_delete(&mut self) -> Vec<Action> {
        let Some(m) = self.view.ws_menu.as_mut() else {
            return Vec::new();
        };
        let id = m.id.clone();
        let only = !self.tabs.iter().any(|t| t.name != id);
        let m = self.view.ws_menu.as_mut().expect("open");
        m.sel = Choice::Cancel;
        m.scroll = 0;
        m.armed = false;
        if only {
            m.step = Step::Plan(RemovalPlan {
                refused: Some(format!(
                    "it is the only workspace, and the TUI would have none to show. Make \
                     another first (+ New workspace), or quit and run `twr ws rm {id}`: `twr` \
                     then asks for a new one"
                )),
                ..RemovalPlan::default()
            });
            m.shown_at = Some(Instant::now());
            return Vec::new();
        }
        m.step = Step::Checking;
        vec![Action::WsPlan { id }]
    }

    /// What deleting workspace `id` would do came ([`Action::WsPlan`]): shown, if the menu
    /// still waits for it.
    pub fn ws_planned(&mut self, id: &str, plan: anyhow::Result<RemovalPlan>, now: Instant) {
        let Some(m) = self.view.ws_menu.as_mut().filter(|m| m.id == id) else {
            return;
        };
        if m.step != Step::Checking {
            return;
        }
        let plan = plan.unwrap_or_else(|e| RemovalPlan {
            refused: Some(format!("{e:#}")),
            ..RemovalPlan::default()
        });
        m.step = Step::Plan(plan);
        m.sel = Choice::Cancel;
        m.scroll = 0;
        m.armed = false;
        m.shown_at = Some(now);
    }

    /// Runs `choice` of the menu (Enter on it, a tap, `y`).
    fn ws_menu_run(&mut self, choice: Choice, now: Instant) -> Vec<Action> {
        let Some(m) = self.view.ws_menu.clone() else {
            return Vec::new();
        };
        let label = self.ws_label(&m.id);
        match choice {
            Choice::Cancel => {
                self.view.ws_menu = None;
                self.view.status = Some("cancelled".into());
                Vec::new()
            }
            Choice::Open => {
                self.view.ws_menu = None;
                self.switch_workspace(&m.id)
            }
            Choice::Rename => {
                if let Some(menu) = self.view.ws_menu.as_mut() {
                    menu.step = Step::Rename(label);
                    menu.sel = Choice::SaveName;
                }
                Vec::new()
            }
            Choice::SaveName => {
                let Step::Rename(name) = &m.step else {
                    return Vec::new();
                };
                if *name == label {
                    self.view.ws_menu = None;
                    self.view.status = Some(format!("workspace {label} is named {name} already"));
                    return Vec::new();
                }
                if let Some(why) = self.ws_name_error(Some(&m.id), name) {
                    self.view.status = Some(why);
                    return Vec::new();
                }
                self.view.ws_menu = None;
                self.view.status = Some(format!("renaming workspace {label}..."));
                vec![Action::WsRename {
                    id: m.id,
                    to: name.clone(),
                }]
            }
            Choice::MakeDefault => {
                self.view.ws_menu = None;
                self.view.status = Some(format!("making {label} the default workspace..."));
                vec![Action::WsDefault { id: m.id }]
            }
            Choice::Delete => self.ws_menu_delete(),
            Choice::ConfirmDelete => {
                let Step::Plan(plan) = &m.step else {
                    return Vec::new();
                };
                if plan.refused.is_some() {
                    return Vec::new();
                }
                if m.shown_at
                    .is_some_and(|at| now.duration_since(at) < CARD_SETTLE)
                {
                    self.view.status = Some("read it first: it just came".into());
                    return Vec::new();
                }
                if let Some(to) = &self.switching {
                    let to = self.ws_label(to);
                    self.view.status = Some(format!("already opening workspace {to}"));
                    return Vec::new();
                }
                let via = if self.workspace.as_deref() == Some(m.id.as_str()) {
                    match self.ws_removal_via(&m.id) {
                        Some(to) => Some(to),
                        None => {
                            self.view.status =
                                Some("it is the only workspace: make another first".into());
                            return Vec::new();
                        }
                    }
                } else {
                    None
                };
                self.view.ws_menu = None;
                self.view.status = Some(match &via {
                    Some(to) => format!(
                        "switching to {} to delete workspace {label}...",
                        self.ws_label(to)
                    ),
                    None => format!("deleting workspace {label}..."),
                });
                vec![Action::WsRemove {
                    id: m.id,
                    stop_hub: plan.stops_hub,
                    via,
                }]
            }
        }
    }

    /// The menu's keys: ↑↓ move the highlight, Enter runs it, Esc closes; in the plan
    /// PgUp/PgDn/Home/End scroll it, `y` deletes and `n` cancels; the rename takes a name's
    /// characters and its editing keys. It takes every key.
    pub(crate) fn ws_menu_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let choices: Vec<Choice> = self.ws_menu_choices().into_iter().map(|(c, _)| c).collect();
        let max_scroll = self.view.panes.ws_menu_scroll;
        let Some(m) = self.view.ws_menu.as_mut() else {
            return Vec::new();
        };
        let at = choices.iter().position(|c| *c == m.sel).unwrap_or(0);
        match key.code {
            KeyCode::Esc => {
                self.view.ws_menu = None;
                self.view.status = Some("cancelled".into());
                return Vec::new();
            }
            KeyCode::Enter if !ctrl && !alt => {
                let choice = choices.get(at).copied().unwrap_or(Choice::Cancel);
                return self.ws_menu_run(choice, Instant::now());
            }
            KeyCode::Up | KeyCode::BackTab if !choices.is_empty() => {
                m.sel = choices[at.saturating_sub(1)];
                m.armed = false;
            }
            KeyCode::Down | KeyCode::Tab if !choices.is_empty() => {
                m.sel = choices[(at + 1).min(choices.len() - 1)];
                m.armed = false;
            }
            KeyCode::PageUp => m.scroll = m.scroll.saturating_sub(5),
            KeyCode::PageDown => m.scroll = (m.scroll + 5).min(max_scroll),
            KeyCode::Home if !matches!(m.step, Step::Rename(_)) => m.scroll = 0,
            KeyCode::End if !matches!(m.step, Step::Rename(_)) => m.scroll = max_scroll,
            _ => {}
        }
        match &mut m.step {
            Step::Rename(name) => match key.code {
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char('u' | 'w') if ctrl => name.clear(),
                KeyCode::Char('h') if ctrl => {
                    name.pop();
                }
                KeyCode::Char(c)
                    if !ctrl && !alt && !c.is_control() && name.chars().count() < NAME_MAX =>
                {
                    name.push(c);
                }
                _ => {}
            },
            Step::Plan(_) => match key.code {
                KeyCode::Char('y') if !ctrl && !alt => {
                    if choices.contains(&Choice::ConfirmDelete) {
                        return self.ws_menu_run(Choice::ConfirmDelete, Instant::now());
                    }
                }
                KeyCode::Char('n') if !ctrl && !alt => {
                    return self.ws_menu_run(Choice::Cancel, Instant::now());
                }
                _ => {}
            },
            Step::Choices => {
                // The first letter of a choice runs it: o, r, m, d.
                let by_key = match key.code {
                    KeyCode::Char('o') => Some(Choice::Open),
                    KeyCode::Char('r') => Some(Choice::Rename),
                    KeyCode::Char('m') => Some(Choice::MakeDefault),
                    KeyCode::Char('d') => Some(Choice::Delete),
                    _ => None,
                };
                if let Some(c) = by_key.filter(|c| !ctrl && !alt && choices.contains(c)) {
                    return self.ws_menu_run(c, Instant::now());
                }
            }
            Step::Checking => {}
        }
        Vec::new()
    }

    /// A paste into the rename: its first line, its control characters left out.
    pub(crate) fn ws_menu_paste(&mut self, text: &str) {
        let Some(Step::Rename(name)) = self.view.ws_menu.as_mut().map(|m| &mut m.step) else {
            return;
        };
        let line = text.lines().next().unwrap_or_default().trim();
        for c in line.chars().filter(|c| !c.is_control()) {
            if name.chars().count() >= NAME_MAX {
                break;
            }
            name.push(c);
        }
    }

    /// A tap on choice `choice`: runs it; Delete only on a second tap, once the first
    /// highlighted it.
    pub(crate) fn tap_ws_choice(&mut self, choice: Choice) -> Vec<Action> {
        let Some(m) = self.view.ws_menu.as_mut() else {
            return Vec::new();
        };
        if choice == Choice::ConfirmDelete && !(m.armed && m.sel == choice) {
            m.sel = choice;
            m.armed = true;
            self.view.status = Some("tap Delete again to delete the workspace".into());
            return Vec::new();
        }
        self.ws_menu_run(choice, Instant::now())
    }

    /// The wheel over the menu: scrolls the plan, else moves the highlight.
    pub(crate) fn ws_menu_wheel(&mut self, up: bool) -> Vec<Action> {
        let max_scroll = self.view.panes.ws_menu_scroll;
        if max_scroll > 0 {
            if let Some(m) = self.view.ws_menu.as_mut() {
                m.scroll = if up {
                    m.scroll.saturating_sub(1)
                } else {
                    (m.scroll + 1).min(max_scroll)
                };
            }
            return Vec::new();
        }
        let code = if up { KeyCode::Up } else { KeyCode::Down };
        self.ws_menu_key(KeyEvent::new(code, KeyModifiers::NONE))
    }
}

/// Draws the workspace menu over `area`: its title (the workspace, `✕` closing it), its text
/// (a plan scrolls, the rows below it saying so), then its choices, which stay on screen: a
/// row each, on a phone a band two rows tall ([`crate::ui::TAP_ROWS`]), each its tap target.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let Some(m) = state.view.ws_menu.as_ref() else {
        return;
    };
    let phone = state.compact();
    let width = area.width.min(MAX_WIDTH);
    let inner_w = usize::from(width.saturating_sub(2)).max(1);
    let dim = Style::default().fg(Color::DarkGray);
    let label = state.ws_label(&m.id);
    let plan = matches!(m.step, Step::Plan(_) | Step::Checking);
    let title = if plan {
        format!(" delete workspace {label} ")
    } else {
        format!(" workspace {label} ")
    };
    // Its text, wrapped.
    let mut text: Vec<Line<'static>> = Vec::new();
    for (i, t) in state.ws_menu_text().into_iter().enumerate() {
        let style = match &m.step {
            Step::Choices => dim,
            Step::Rename(_) if i == 0 => Style::default().add_modifier(Modifier::BOLD),
            Step::Rename(name) if i == 2 && state.ws_name_error(Some(&m.id), name).is_some() => {
                Style::default().fg(Color::Red)
            }
            Step::Plan(p) if i == 0 && p.refused.is_some() => Style::default().fg(Color::Yellow),
            Step::Plan(_) if i == 0 => Style::default().add_modifier(Modifier::BOLD),
            _ => dim,
        };
        if t.is_empty() {
            text.push(Line::default());
            continue;
        }
        text.extend(crate::ui::wrap_line(
            Line::from(Span::styled(format!(" {t}"), style)),
            inner_w,
        ));
    }
    // Its choices: a row each, a band on a phone.
    let tall = if phone {
        usize::from(crate::ui::TAP_ROWS)
    } else {
        1
    };
    let choices = state.ws_menu_choices();
    let choice_rows = choices.len() * tall;
    let hint = if phone {
        None
    } else {
        Some(match &m.step {
            Step::Plan(p) if p.refused.is_some() => "Enter or Esc closes",
            Step::Plan(_) => "↑↓ Enter · y deletes · PgUp/PgDn scroll · Esc cancels",
            Step::Rename(_) => "Enter renames · Esc cancels",
            _ => "↑↓ Enter (or o r m d) · Esc closes",
        })
    };
    let gap = usize::from(!text.is_empty());
    let want = text.len() + gap + choice_rows + usize::from(hint.is_some()) + 2;
    let height = u16::try_from(want)
        .unwrap_or(u16::MAX)
        .min(area.height)
        .max(3);
    let rect = crate::ui::centered(area, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if plan { Color::Yellow } else { Color::Cyan }))
        .title(crate::ui::truncate_cells(&title, inner_w.saturating_sub(3)))
        .title(crate::ui::close_title());
    let inner = block.inner(rect);
    panes.cover(None);
    panes.add(PaneId::Text, inner);
    crate::ui::close_row(panes, rect, Target::Close);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    if inner.height == 0 {
        return;
    }
    // The choices and the hint keep their rows; the text gets the rest, scrolled.
    let below = (choice_rows + usize::from(hint.is_some())).min(usize::from(inner.height));
    let text_room = usize::from(inner.height) - below;
    let text_rows = text_room.saturating_sub(gap.min(text_room.saturating_sub(1)));
    let cut = text.len() > text_rows;
    // A cut text says what is below or above on its last row.
    let shown_rows = if cut {
        text_rows.saturating_sub(1)
    } else {
        text_rows
    };
    let max_scroll = text.len().saturating_sub(shown_rows);
    let scroll = m.scroll.min(max_scroll);
    let mut y = inner.y;
    let shown: Vec<Line<'static>> = text.iter().skip(scroll).take(shown_rows).cloned().collect();
    let n = u16::try_from(shown.len()).unwrap_or(0);
    frame.render_widget(Paragraph::new(shown), Rect::new(inner.x, y, inner.width, n));
    y += n;
    if cut && text_rows > 0 {
        let rest = text.len() - scroll - shown_rows.min(text.len() - scroll);
        let cue = if rest > 0 {
            format!(" ↓ {rest} more lines (wheel, PgDn)")
        } else {
            format!(" ↑ {scroll} lines above (wheel, PgUp)")
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_cells(&cue, inner_w),
                Style::default().fg(Color::Yellow),
            )),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }
    panes.ws_menu_scroll = max_scroll;
    let top = inner.bottom() - u16::try_from(below).unwrap_or(0);
    y = y.max(top);
    for (i, (choice, said)) in choices.iter().enumerate() {
        let room = inner.bottom().saturating_sub(y);
        let h = u16::try_from(tall).unwrap_or(1);
        if room < h {
            break;
        }
        let selected = *choice == m.sel;
        let danger = *choice == Choice::ConfirmDelete;
        let mut style = if selected {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        if danger {
            style = style.fg(Color::Red);
        }
        let mark = if selected && danger && m.armed {
            " ▶ "
        } else {
            "   "
        };
        let line = Line::from(Span::styled(format!("{mark}{said}"), style));
        let lines = if phone {
            let bg = if i % 2 == 0 {
                crate::ui::BAND
            } else {
                crate::ui::BAND_ALT
            };
            crate::ui::tap_band(vec![line], inner_w, bg)
        } else {
            let fill = inner_w.saturating_sub(line.width());
            let mut line = line;
            line.spans.push(Span::styled(" ".repeat(fill), style));
            vec![line]
        };
        let r = Rect::new(inner.x, y, inner.width, h);
        frame.render_widget(Paragraph::new(lines), r);
        panes.target(r, Target::WsChoice(*choice));
        y += h;
    }
    if let Some(hint) = hint {
        if y < inner.bottom() {
            frame.render_widget(
                Paragraph::new(Span::styled(crate::ui::truncate_cells(hint, inner_w), dim)),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }
    // The cursor after the name being typed.
    if let Step::Rename(name) = &m.step {
        let x = inner.x + 7 + u16::try_from(crate::edit::width(name)).unwrap_or(u16::MAX);
        if x < inner.right() {
            frame.set_cursor_position((x, inner.y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::key;
    use crate::sidebar::tests::{cells, in_workspaces, rows, tap};

    const DESK: (u16, u16) = (160, 40);

    fn text(st: &mut AppState, (w, h): (u16, u16)) -> String {
        rows(st, w, h).join("\n")
    }

    fn type_in(st: &mut AppState, s: &str) {
        for c in s.chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
    }

    /// A plan as the CLI says it, `n` lines long.
    fn plan(n: usize, stops_hub: bool) -> RemovalPlan {
        RemovalPlan {
            refused: None,
            lines: (0..n).map(|i| format!("plan line {i}")).collect(),
            stops_hub,
        }
    }

    /// Opens scratch's Delete and gives it `p`, settled.
    fn delete_with(st: &mut AppState, id: &str, p: RemovalPlan) {
        let a = st.ws_rm_cmd(id);
        assert_eq!(a, [Action::WsPlan { id: id.into() }]);
        let long_ago = Instant::now() - CARD_SETTLE * 2;
        st.ws_planned(id, Ok(p), long_ago);
    }

    fn removal(actions: &[Action]) -> Option<(String, bool, Option<String>)> {
        actions.iter().find_map(|a| match a {
            Action::WsRemove { id, stop_hub, via } => Some((id.clone(), *stop_hub, via.clone())),
            _ => None,
        })
    }

    /// Every workspace card has a `⋯` at the end of its name row; a tap on it opens that
    /// workspace's menu (not a switch), whose choices run: Open switches, Esc closes.
    #[test]
    fn a_cards_dots_open_its_menu() {
        let mut st = in_workspaces("default");
        let r = text(&mut st, DESK);
        assert!(r.contains(" client-x") && r.contains("⋯"), "{r}");
        for id in ["default", "client-x", "scratch", "docs"] {
            let dots = cells(&mut st, DESK.0, DESK.1, &Target::WsMenu(id.into()));
            assert_eq!(dots.len(), 1, "{id}");
            assert_eq!(dots[0].width, 3, "{id}");
        }
        let a = tap(&mut st, DESK.0, DESK.1, &Target::WsMenu("client-x".into()));
        assert!(a.is_empty(), "{a:?}");
        assert_eq!(
            st.view.ws_menu.as_ref().map(|m| m.id.as_str()),
            Some("client-x")
        );
        let r = text(&mut st, DESK);
        for want in [
            "workspace client-x",
            "Open client-x",
            "Rename…",
            "Make default",
            "Delete…",
        ] {
            assert!(r.contains(want), "{want}: {r}");
        }
        // Under the menu a card does not switch.
        let a = tap(
            &mut st,
            DESK.0,
            DESK.1,
            &Target::Workspace("scratch".into()),
        );
        assert!(a.is_empty() && st.view.ws_menu.is_some(), "{a:?}");
        let a = tap(&mut st, DESK.0, DESK.1, &Target::WsChoice(Choice::Open));
        assert_eq!(
            a,
            [Action::Workspace {
                name: "client-x".into(),
                create: false
            }]
        );
        assert!(st.view.ws_menu.is_none());
        st.switching = None;
        st.open_ws_menu(Some("scratch"));
        st.handle_key(key(KeyCode::Esc));
        assert!(st.view.ws_menu.is_none());
        assert_eq!(st.view.status.as_deref(), Some("cancelled"));
        // The current workspace's menu has no Open; the default's no Make default.
        st.view.status = None;
        st.tabs[0].default = true;
        st.open_ws_menu(None);
        let choices: Vec<Choice> = st.ws_menu_choices().into_iter().map(|(c, _)| c).collect();
        assert_eq!(choices, [Choice::Rename, Choice::Delete]);
        assert!(text(&mut st, DESK).contains("the default workspace · open here"));
    }

    /// Rename asks for the new name with the rules `twr ws rename` has: not another's name
    /// or original name, and `default` only for the root's; Enter renames, by id.
    #[test]
    fn rename_checks_the_name_and_renames_by_id() {
        let mut st = in_workspaces("default");
        // client-x was renamed acme: client-x is still its own.
        st.tabs[1].label = "acme".into();
        st.open_ws_menu(Some("acme"));
        assert_eq!(st.view.ws_menu.as_ref().unwrap().id, "client-x");
        let a = st.handle_key(key(KeyCode::Char('r')));
        assert!(a.is_empty());
        assert!(text(&mut st, DESK).contains("name: acme"));
        for (bad, why) in [
            ("scratch", "there is a workspace scratch already"),
            ("Bad", "invalid workspace name"),
            ("default", "there is a workspace default already"),
        ] {
            st.handle_key(crate::app::tests::ctrl('u'));
            type_in(&mut st, bad);
            assert!(text(&mut st, DESK).contains(why), "{bad}");
            let a = st.handle_key(key(KeyCode::Enter));
            assert!(a.is_empty(), "{bad}");
            assert!(st.view.ws_menu.is_some(), "{bad}: the menu stays");
        }
        // Back to its original name: fine.
        st.handle_key(crate::app::tests::ctrl('u'));
        type_in(&mut st, "client-x");
        let a = st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            [Action::WsRename {
                id: "client-x".into(),
                to: "client-x".into()
            }]
        );
        // Another can't take an original name; `/ws rename` goes straight to the field.
        let a = st.ws_rename_cmd(Some("scratch"), "client-x");
        assert!(a.is_empty());
        assert!(text(&mut st, DESK).contains("original name of workspace acme"));
        st.handle_key(key(KeyCode::Esc));
        st.ws_rename_cmd(None, "main");
        let a = st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            a,
            [Action::WsRename {
                id: "default".into(),
                to: "main".into()
            }]
        );
        // Make default runs at once, by id.
        st.open_ws_menu(Some("scratch"));
        let a = st.handle_key(key(KeyCode::Char('m')));
        assert_eq!(
            a,
            [Action::WsDefault {
                id: "scratch".into()
            }]
        );
    }

    /// Delete says what deleting would remove (the CLI's plan) under a `Delete` and a
    /// `Cancel` that stay on screen however long it is: it scrolls above them. It opens on
    /// Cancel; Delete runs on `y` or Enter on it once the plan has been on screen a moment, or
    /// a second tap on it.
    #[test]
    fn delete_shows_the_plan_and_asks() {
        let mut st = in_workspaces("default");
        let a = st.ws_rm_cmd("scratch");
        assert_eq!(
            a,
            [Action::WsPlan {
                id: "scratch".into()
            }]
        );
        assert!(text(&mut st, DESK).contains("finding out what deleting scratch would remove"));
        // A plan for another workspace, or one that came after Cancel, is not shown.
        st.ws_planned("docs", Ok(plan(3, false)), Instant::now());
        assert_eq!(st.view.ws_menu.as_ref().unwrap().step, Step::Checking);
        st.ws_planned("scratch", Ok(plan(60, true)), Instant::now());
        let r = text(&mut st, (160, 24));
        assert!(
            r.contains("Delete workspace scratch? This cannot be undone."),
            "{r}"
        );
        assert!(r.contains("deleting stops it first"), "{r}");
        assert!(r.contains("more lines"), "{r}");
        assert!(!r.contains("plan line 59"), "{r}");
        for c in [Choice::ConfirmDelete, Choice::Cancel] {
            let at = cells(&mut st, 160, 24, &Target::WsChoice(c));
            assert_eq!(at.len(), 1, "{c:?}");
            assert!(at[0].bottom() <= 24, "{c:?}");
        }
        // Just came: not yet.
        let a = st.handle_key(key(KeyCode::Char('y')));
        assert!(removal(&a).is_none());
        assert!(st.view.status.as_deref().unwrap().contains("it just came"));
        // Scrolled to its end, the choices still there.
        for _ in 0..20 {
            st.handle_key(key(KeyCode::PageDown));
        }
        let r = text(&mut st, (160, 24));
        assert!(
            r.contains("plan line 59") && r.contains("lines above"),
            "{r}"
        );
        assert!(r.contains("Delete scratch") && r.contains("Cancel"), "{r}");
        // Enter alone cancels.
        let a = st.handle_key(key(KeyCode::Enter));
        assert!(a.is_empty() && st.view.ws_menu.is_none());
        delete_with(&mut st, "scratch", plan(4, false));
        let a = st.handle_key(key(KeyCode::Char('y')));
        assert_eq!(removal(&a), Some(("scratch".into(), false, None)));
        assert!(st.view.ws_menu.is_none());
        // A tap highlights Delete; a second one deletes.
        delete_with(&mut st, "docs", plan(4, true));
        let a = tap(
            &mut st,
            DESK.0,
            DESK.1,
            &Target::WsChoice(Choice::ConfirmDelete),
        );
        assert!(a.is_empty());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("tap Delete again"));
        let a = tap(
            &mut st,
            DESK.0,
            DESK.1,
            &Target::WsChoice(Choice::ConfirmDelete),
        );
        assert_eq!(removal(&a), Some(("docs".into(), true, None)));
    }

    /// A plan that refuses says why and what to do, with Close only; the only workspace
    /// there is is refused here at once; a TUI an agent runs opens no menu.
    #[test]
    fn refusals() {
        let mut st = in_workspaces("default");
        let a = st.ws_rm_cmd("client-x");
        assert_eq!(a.len(), 1);
        st.ws_planned(
            "client-x",
            Ok(RemovalPlan {
                refused: Some("workspace client-x still has 2 threads (api, web)".into()),
                ..RemovalPlan::default()
            }),
            Instant::now() - CARD_SETTLE * 2,
        );
        let r = text(&mut st, DESK);
        assert!(r.contains("client-x can't be deleted now:"), "{r}");
        assert!(r.contains("still has 2"), "{r}");
        let choices: Vec<Choice> = st.ws_menu_choices().into_iter().map(|(c, _)| c).collect();
        assert_eq!(choices, [Choice::Cancel]);
        assert!(removal(&st.handle_key(key(KeyCode::Char('y')))).is_none());
        st.handle_key(key(KeyCode::Esc));
        // An error finding out is a refusal too.
        st.ws_rm_cmd("docs");
        st.ws_planned(
            "docs",
            Err(anyhow::anyhow!("its hub did not answer")),
            Instant::now(),
        );
        assert!(text(&mut st, DESK).contains("its hub did not answer"));
        st.handle_key(key(KeyCode::Esc));

        let mut alone = in_workspaces("default");
        let only = alone.tabs[0].clone();
        alone.set_tabs(vec![only]);
        let a = alone.ws_rm_cmd("default");
        assert!(a.is_empty(), "{a:?}");
        assert!(text(&mut alone, DESK).contains("it is the only workspace"));

        let mut agent = in_workspaces("default");
        agent.agent = Some("thread api".into());
        let a = tap(
            &mut agent,
            DESK.0,
            DESK.1,
            &Target::WsMenu("scratch".into()),
        );
        assert!(a.is_empty());
        assert!(agent.view.ws_menu.is_none());
        assert!(agent
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("agents cannot rename, delete"));
        assert!(agent.ws_rm_cmd("scratch").is_empty() && agent.view.ws_menu.is_none());
    }

    /// Deleting the workspace shown goes to the default first (else the next one), and says
    /// so before it asks.
    #[test]
    fn deleting_the_current_workspace_switches_away_first() {
        let mut st = in_workspaces("client-x");
        st.tabs[2].default = true;
        delete_with(&mut st, "client-x", plan(2, true));
        assert!(text(&mut st, DESK).contains("You are in it: the TUI switches to scratch first."));
        let a = st.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            removal(&a),
            Some(("client-x".into(), true, Some("scratch".into())))
        );
        let mut st = in_workspaces("default");
        delete_with(&mut st, "default", plan(2, false));
        let a = st.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            removal(&a),
            Some(("default".into(), false, Some("client-x".into())))
        );
    }

    /// On a phone the cards' `⋯` and the menu's choices are menu choices, two rows tall, and a
    /// long plan scrolls above Delete and Cancel, which stay whole on screen at every size;
    /// the wheel scrolls it.
    #[test]
    fn on_a_phone() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let mut st = in_workspaces("default");
        st.show_pane(crate::app::CompactPane::Threads);
        for (w, h) in [(40, 20), (50, 14), (40, 12), (50, 30)] {
            let dots = cells(&mut st, w, h, &Target::WsMenu("client-x".into()));
            assert!(st.compact(), "{w}x{h}");
            if let Some(d) = dots.first() {
                assert!(d.height >= 2, "{w}x{h}: {d:?}");
            }
        }
        let a = tap(&mut st, 40, 20, &Target::WsMenu("default".into()));
        assert!(a.is_empty());
        for c in [Choice::Rename, Choice::MakeDefault, Choice::Delete] {
            let at = cells(&mut st, 40, 20, &Target::WsChoice(c));
            assert_eq!(at.len(), 1, "{c:?}");
            assert!(at[0].height >= 2, "{c:?}: {at:?}");
        }
        st.view.ws_menu = None;
        delete_with(&mut st, "scratch", plan(40, true));
        for (w, h) in [(40, 20), (40, 12), (60, 10), (40, 9)] {
            let r = rows(&mut st, w, h).join("\n");
            let mut seen = Vec::new();
            for c in [Choice::ConfirmDelete, Choice::Cancel] {
                let at = cells(&mut st, w, h, &Target::WsChoice(c));
                assert_eq!(at.len(), 1, "{c:?} at {w}x{h}: {r}");
                assert!(at[0].height >= 2 && at[0].bottom() <= h, "{c:?} at {w}x{h}");
                seen.push(at[0]);
            }
            assert!(!seen[0].intersects(seen[1]), "{w}x{h}");
            assert!(r.contains("more lines"), "{w}x{h}: {r}");
        }
        let first = |st: &mut AppState| rows(st, 40, 20).join("\n");
        let before = first(&mut st);
        let screen = crate::app::tests::draw(&mut st, 40, 20);
        let at = cells(&mut st, 40, 20, &Target::WsChoice(Choice::Cancel))[0];
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: at.x + 1,
            row: at.y.saturating_sub(3),
            modifiers: KeyModifiers::NONE,
        };
        for _ in 0..3 {
            st.handle_mouse(wheel, &screen);
        }
        assert_ne!(first(&mut st), before);
        // A tap on Cancel's second row cancels.
        let at = cells(&mut st, 40, 20, &Target::WsChoice(Choice::Cancel))[0];
        let a = crate::sidebar::tests::tap_at(&mut st, 40, 20, at.x + 1, at.y + 1);
        assert!(a.is_empty() && st.view.ws_menu.is_none());
    }
}
