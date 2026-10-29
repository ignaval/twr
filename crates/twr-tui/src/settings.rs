//! The settings overlay (`/settings`, `⚙ Settings` under the workspaces): the settings users
//! change most, as they are in effect, each with where it is kept.
//!
//! The TUI's own settings change at once and are written back where they are kept:
//! suggestions, summaries, the card order, the colors, terminal notifications and the compact
//! width to the config file's `[tui]` table (`[tui.notify]` for notifications), by [`crate::config_edit`], which
//! changes that one key and nothing else of the file (its comments, its layout, its other
//! tables and their secrets stay byte for byte as they were) or, when it can't be sure of
//! that, leaves the file alone and says so: the change then holds for this session only. The
//! workspace sidebar is the TUI's own state, kept with what it remembers across runs
//! (`tui-state.json`, [`crate::firstrun`]), as Ctrl-T keeps it.
//!
//! The suggestion models (a table or a string in the file), `[tui] summary_every`,
//! `[codex] auto` and `wake_after_secs` are shown, not changed: the models are free text with
//! a size check of their own, the summaries' pace a span the file spells, and the last two are the hub's, read when it starts (as `config.show` reports
//! them): a change needs the file edited and the hub restarted, and `[codex] auto` decides what
//! Codex threads run without asking, which no stray tap should change. A TUI an agent runs
//! changes nothing.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use serde_json::Value;
use twr_core::config::{CardOrder, ColorMode, TerminalAlert};

use crate::app::AppState;
use crate::select::{PaneId, Panes, Target};

/// A row of the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// `[tui] suggest`.
    Suggest,
    /// `[tui] suggest_model` (shown only).
    SuggestModel,
    /// `[tui] summaries`.
    Summaries,
    /// `[tui] summary_every` (shown only).
    SummaryEvery,
    /// `[tui] card_order`.
    CardOrder,
    /// `[tui.notify] terminal`.
    Notify,
    /// `[tui] compact_width`.
    CompactWidth,
    /// The workspace sidebar (`tui-state.json`).
    WsSidebar,
    /// `[tui] colors`.
    Colors,
    /// `[codex] auto`, the hub's (shown only).
    CodexAuto,
    /// `wake_after_secs`, the hub's (shown only).
    WakeAfter,
}

/// The rows, in order.
pub const KEYS: [Key; 11] = [
    Key::Suggest,
    Key::SuggestModel,
    Key::Summaries,
    Key::SummaryEvery,
    Key::CardOrder,
    Key::Notify,
    Key::CompactWidth,
    Key::WsSidebar,
    Key::Colors,
    Key::CodexAuto,
    Key::WakeAfter,
];

/// The compact widths a change steps through (0: never compact).
pub const COMPACT_WIDTHS: [u16; 7] = [0, 40, 50, 60, 80, 100, 120];

/// The color modes a change steps through.
const COLOR_MODES: [ColorMode; 4] = [
    ColorMode::Auto,
    ColorMode::Full,
    ColorMode::Basic,
    ColorMode::None,
];

/// The terminal alerts a change steps through.
const ALERTS: [TerminalAlert; 4] = [
    TerminalAlert::Off,
    TerminalAlert::Bell,
    TerminalAlert::Osc9,
    TerminalAlert::Osc777,
];

/// A change of a setting, applied to the view at once and then kept where it is kept
/// ([`Change::config_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Suggestions on or off.
    Suggest(bool),
    /// Summaries on or off.
    Summaries(bool),
    /// The colors the TUI draws with.
    Colors(ColorMode),
    /// The card order.
    CardOrder(CardOrder),
    /// How the terminal is alerted.
    Notify(TerminalAlert),
    /// The compact width.
    CompactWidth(u16),
    /// The workspace sidebar shown (`true`) or hidden.
    WsSidebar(bool),
}

impl Change {
    /// Where in the config file it is kept: the table, the key and the value. `None` for the
    /// workspace sidebar, kept in `tui-state.json`.
    pub fn config_key(
        &self,
    ) -> Option<(
        &'static [&'static str],
        &'static str,
        crate::config_edit::Scalar,
    )> {
        use crate::config_edit::Scalar;
        const TUI: &[&str] = &["tui"];
        const NOTIFY: &[&str] = &["tui", "notify"];
        Some(match self {
            Change::Suggest(on) => (TUI, "suggest", Scalar::Bool(*on)),
            Change::Summaries(on) => (TUI, "summaries", Scalar::Bool(*on)),
            Change::Colors(m) => (TUI, "colors", Scalar::Str(color_mode_str(*m).into())),
            Change::CardOrder(o) => (TUI, "card_order", Scalar::Str(card_order_str(*o).into())),
            Change::Notify(a) => (NOTIFY, "terminal", Scalar::Str(alert_str(*a).into())),
            Change::CompactWidth(w) => (TUI, "compact_width", Scalar::Int(i64::from(*w))),
            Change::WsSidebar(_) => return None,
        })
    }
}

/// The overlay's state while it is open.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    /// The selected row.
    pub selected: usize,
    /// The hub's `config.show` (`None` until it answered): its `[codex] auto` and
    /// `wake_after_secs`.
    pub hub: Option<Value>,
    /// What the config file says of the TUI's keys now, read (off the loop) when the overlay
    /// opened and after each write: a value shown when it differs from the one in effect.
    pub file: Vec<(Key, Option<String>)>,
    /// The file's path, as shown (`~` for the home directory), when the environment names one.
    pub path: Option<String>,
}

impl Settings {
    /// The selected row's key.
    pub fn key(&self) -> Key {
        KEYS[self.selected.min(KEYS.len() - 1)]
    }

    /// What the file said of `key` when last read.
    fn file_value(&self, key: Key) -> Option<&str> {
        self.file
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.as_deref())
    }
}

/// `v` as the config file writes it, for the status bar.
pub fn scalar_text(v: &crate::config_edit::Scalar) -> String {
    use crate::config_edit::Scalar;
    match v {
        Scalar::Bool(b) => b.to_string(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Str(s) => format!("{s:?}"),
    }
}

/// `card_order` as the file writes it.
fn card_order_str(o: CardOrder) -> &'static str {
    match o {
        CardOrder::Stable => "stable",
        CardOrder::Attention => "attention",
    }
}

/// `[tui] colors` as the file writes it.
fn color_mode_str(m: ColorMode) -> &'static str {
    match m {
        ColorMode::Auto => "auto",
        ColorMode::Full => "256",
        ColorMode::Basic => "16",
        ColorMode::None => "none",
    }
}

/// The colors drawn with, as shown: the mode, and for `auto` what the environment made of it.
fn colors_text(m: ColorMode, depth: crate::color::Depth) -> String {
    use crate::color::Depth;
    let depth = match depth {
        Depth::Full => "256 colors",
        Depth::Basic => "16 colors",
        Depth::None => "no color",
    };
    match m {
        ColorMode::Auto => format!("auto ({depth})"),
        _ => depth.into(),
    }
}

/// `[tui.notify] terminal` as the file writes it.
fn alert_str(a: TerminalAlert) -> &'static str {
    match a {
        TerminalAlert::Off => "off",
        TerminalAlert::Bell => "bell",
        TerminalAlert::Osc9 => "osc9",
        TerminalAlert::Osc777 => "osc777",
    }
}

/// The compact width as shown.
fn width_text(w: u16) -> String {
    if w == 0 {
        "0 (never)".into()
    } else {
        format!("{w} columns")
    }
}

/// What the config file says of the TUI's keys, as the overlay shows it (`None`: unset, or
/// no file), read once. It may wait on a writer: never on the IO loop (the settings writer
/// reads it, [`crate::app::Action::SettingsFile`]).
pub fn read_file(path: &std::path::Path) -> Vec<(Key, Option<String>)> {
    let doc = crate::config_edit::read_doc(path).unwrap_or_default();
    let read = |table: &[&str], key: &str| {
        crate::config_edit::key_of(&doc, table, key).map(|v| match v {
            toml::Value::String(s) => s,
            other => other.to_string(),
        })
    };
    vec![
        (Key::Suggest, read(&["tui"], "suggest")),
        (Key::Summaries, read(&["tui"], "summaries")),
        (Key::Colors, read(&["tui"], "colors")),
        (Key::CardOrder, read(&["tui"], "card_order")),
        (Key::Notify, read(&["tui", "notify"], "terminal")),
        (Key::CompactWidth, read(&["tui"], "compact_width")),
    ]
}

/// One row as drawn: its label, the value in effect, where it is kept, and whether a change
/// is possible here.
struct Row {
    key: Key,
    label: &'static str,
    value: String,
    kept: String,
    editable: bool,
}

/// The rows for `state` (the overlay `s` open).
fn rows(state: &AppState, s: &Settings) -> Vec<Row> {
    let v = &state.view;
    let agent = state.agent.is_some();
    let hub = s.hub.as_ref();
    let differs = |key: Key, now: &str| match s.file_value(key) {
        Some(f) if f != now => format!(" · the file says {f}"),
        _ => String::new(),
    };
    KEYS.iter()
        .map(|&key| match key {
            Key::Suggest => {
                let now = if v.suggest { "true" } else { "false" };
                Row {
                    key,
                    label: "Suggestions",
                    value: if v.suggest { "on" } else { "off" }.into(),
                    kept: format!("[tui] suggest{}", differs(key, now)),
                    editable: !agent,
                }
            }
            Key::SuggestModel => Row {
                key,
                label: "Suggestion model",
                value: format!(
                    "{} · codex {}",
                    v.suggest_model.claude_code, v.suggest_model.codex
                ),
                kept: "[tui] suggest_model · edit the file to change it".into(),
                editable: false,
            },
            Key::Summaries => {
                let now = if state.summary.on { "true" } else { "false" };
                Row {
                    key,
                    label: "Summaries",
                    value: if state.summary.on { "on" } else { "off" }.into(),
                    kept: format!("[tui] summaries{}", differs(key, now)),
                    editable: !agent,
                }
            }
            Key::SummaryEvery => Row {
                key,
                label: "Summary pace",
                value: format!(
                    "at most once per {} a thread",
                    crate::summary::every_words(state.summary.every())
                ),
                kept: "[tui] summary_every · edit the file to change it".into(),
                editable: false,
            },
            Key::Colors => {
                let now = color_mode_str(v.color_mode);
                Row {
                    key,
                    label: "Colors",
                    value: colors_text(v.color_mode, v.colors),
                    kept: format!("[tui] colors{}", differs(key, now)),
                    editable: !agent,
                }
            }
            Key::CardOrder => {
                let now = card_order_str(v.card_order);
                Row {
                    key,
                    label: "Card order",
                    value: now.into(),
                    kept: format!("[tui] card_order{}", differs(key, now)),
                    editable: !agent,
                }
            }
            Key::Notify => {
                let now = alert_str(v.notify_terminal);
                Row {
                    key,
                    label: "Notifications",
                    value: now.into(),
                    kept: format!("[tui.notify] terminal{}", differs(key, now)),
                    editable: !agent,
                }
            }
            Key::CompactWidth => {
                let now = v.compact_width.to_string();
                let session = match v.compact {
                    Some(true) => " · /compact on for this session",
                    Some(false) => " · /compact off for this session",
                    None => "",
                };
                Row {
                    key,
                    label: "Compact below",
                    value: width_text(v.compact_width),
                    kept: format!("[tui] compact_width{}{session}", differs(key, &now)),
                    editable: !agent,
                }
            }
            Key::WsSidebar => Row {
                key,
                label: "Workspace sidebar",
                value: if v.ws_sidebar_hidden {
                    "hidden"
                } else {
                    "shown"
                }
                .into(),
                kept: "remembered by the TUI (tui-state.json) · Ctrl-T".into(),
                editable: !agent,
            },
            Key::CodexAuto => Row {
                key,
                label: "Codex auto",
                value: hub
                    .and_then(|h| h["codex"]["auto"].as_str())
                    .unwrap_or("…")
                    .into(),
                kept: "[codex] auto · the hub's: edit the file, then restart the hub".into(),
                editable: false,
            },
            Key::WakeAfter => Row {
                key,
                label: "Wake after",
                value: match hub.and_then(|h| h["wake_after_secs"].as_u64()) {
                    Some(0) => "0 (off)".into(),
                    Some(n) => format!("{n} s"),
                    None => "…".into(),
                },
                kept: "wake_after_secs · the hub's: edit the file, then restart the hub".into(),
                editable: false,
            },
        })
        .collect()
}

/// The change a step of the selected row makes from the view `state` shows (`forward`:
/// Enter, →, a tap; else ←), or `None` for a row shown only.
pub fn step(state: &AppState, key: Key, forward: bool) -> Option<Change> {
    let v = &state.view;
    let cycle = |n: usize, at: usize| {
        if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        }
    };
    Some(match key {
        Key::Suggest => Change::Suggest(!v.suggest),
        Key::Summaries => Change::Summaries(!state.summary.on),
        Key::Colors => {
            let at = COLOR_MODES
                .iter()
                .position(|m| *m == v.color_mode)
                .unwrap_or(0);
            Change::Colors(COLOR_MODES[cycle(COLOR_MODES.len(), at)])
        }
        Key::CardOrder => Change::CardOrder(match v.card_order {
            CardOrder::Stable => CardOrder::Attention,
            CardOrder::Attention => CardOrder::Stable,
        }),
        Key::Notify => {
            let at = ALERTS
                .iter()
                .position(|a| *a == v.notify_terminal)
                .unwrap_or(0);
            Change::Notify(ALERTS[cycle(ALERTS.len(), at)])
        }
        Key::CompactWidth => {
            let w = v.compact_width;
            let next = if forward {
                COMPACT_WIDTHS.iter().copied().find(|p| *p > w).unwrap_or(0)
            } else {
                COMPACT_WIDTHS
                    .iter()
                    .rev()
                    .copied()
                    .find(|p| *p < w)
                    .unwrap_or(COMPACT_WIDTHS[COMPACT_WIDTHS.len() - 1])
            };
            Change::CompactWidth(next)
        }
        Key::WsSidebar => Change::WsSidebar(v.ws_sidebar_hidden),
        Key::SuggestModel | Key::SummaryEvery | Key::CodexAuto | Key::WakeAfter => return None,
    })
}

/// Widest the overlay gets.
const MAX_WIDTH: u16 = 84;
/// Columns of a row's label.
const LABEL: usize = 19;

/// The settings overlay, centered over `area`: a row per setting, `label  value`, the place
/// it is kept under it, dim; the selected row reversed; the file's path at the top and the
/// keys at the bottom. Every row of a setting is its tap target ([`Target::Setting`]), its
/// top border closes it.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let Some(s) = state.view.settings.as_ref() else {
        return;
    };
    let width = area.width.min(MAX_WIDTH);
    let inner_w = usize::from(width.saturating_sub(2)).max(1);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = Vec::new();
    // On a phone, a row under the top border closes it too: its `✕` two rows tall, kept
    // above what scrolls.
    let spacer = u16::from(state.compact());
    let path = s.path.as_deref().unwrap_or("no config file here");
    for l in crate::ui::wrap_line(
        Line::from(Span::styled(format!("file: {path}"), dim)),
        inner_w,
    ) {
        lines.push(l);
    }
    lines.push(Line::default());
    let mut spans = Vec::new();
    for (i, row) in rows(state, s).into_iter().enumerate() {
        let start = lines.len();
        let selected = i == s.selected;
        let base = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let label = format!(" {:<w$} ", row.label, w = LABEL);
        let value_style = if row.editable {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        };
        let mut head = Line::from(vec![
            Span::styled(label, base),
            Span::styled(row.value, value_style),
        ]);
        let pad = inner_w.saturating_sub(head.width());
        head.spans.push(Span::styled(" ".repeat(pad), base));
        lines.push(crate::ui::ellipsized(head, inner_w));
        let kept = format!("{:w$}{}", "", row.kept, w = LABEL + 2);
        lines.extend(crate::ui::wrap_line(
            Line::from(Span::styled(kept, dim)),
            inner_w,
        ));
        spans.push((row.key, start..lines.len()));
    }
    lines.push(Line::default());
    let hint = if state.agent.is_some() {
        "read-only: this TUI runs for an agent · Esc closes"
    } else {
        "↑↓ select · Enter or → changes, ← back · Esc closes"
    };
    lines.extend(crate::ui::wrap_line(
        Line::from(Span::styled(hint, dim)),
        inner_w,
    ));
    let rect = crate::ui::centered(
        area,
        width,
        u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2 + spacer),
    );
    // Taller than the screen (a phone), it scrolls to keep the selected row in view.
    let fit = usize::from(rect.height.saturating_sub(2 + spacer));
    let mut scroll = 0;
    if let Some((_, span)) = spans.get(s.selected) {
        if span.end > fit {
            scroll = (span.end - fit).min(span.start);
        }
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" settings ")
        .title(crate::ui::close_title());
    let inner = block.inner(rect);
    let close = Rect {
        height: spacer.min(inner.height),
        ..inner
    };
    let inner = Rect {
        y: inner.y + close.height,
        height: inner.height - close.height,
        ..inner
    };
    panes.cover(None);
    panes.add(PaneId::Text, inner);
    crate::ui::close_row(panes, rect, Target::Close);
    if !close.is_empty() {
        panes.target(close, Target::Close);
    }
    for (key, span) in spans {
        // Its rows on screen, under the scroll; on a phone only one showing two rows at least
        // (or all of its rows) is tapped.
        let (start, end) = (span.start.max(scroll), span.end.min(scroll + fit));
        let whole = start == span.start && end == span.end;
        if start >= end || (spacer > 0 && !whole && end - start < usize::from(crate::ui::TAP_ROWS))
        {
            continue;
        }
        let y = inner
            .y
            .saturating_add(u16::try_from(start - scroll).unwrap_or(u16::MAX));
        let h = u16::try_from(end - start).unwrap_or(u16::MAX);
        let cells = Rect::new(inner.x, y, inner.width, h).intersection(inner);
        panes.target(cells, Target::Setting(key));
    }
    let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    if !close.is_empty() {
        let text = format!("{:>w$}", "✕ close ", w = usize::from(close.width));
        frame.render_widget(Paragraph::new(Span::styled(text, dim)), close);
    }
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

/// `path` as shown: under the home directory, with `~`.
pub fn shown_path(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The summaries and the colors are written as the config file spells them: read back,
    /// the file says what was set.
    #[test]
    fn summaries_and_colors_are_written_as_the_file_spells_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[tui]\nsuggest = true\n").unwrap();
        let load = || twr_core::config::Config::load(&path).unwrap().tui;
        let write = |c: Change| {
            let (table, key, value) = c.config_key().unwrap();
            crate::config_edit::write_key(&path, table, key, &value).unwrap();
        };
        write(Change::Summaries(false));
        assert!(!load().summaries);
        for m in COLOR_MODES {
            write(Change::Colors(m));
            assert_eq!(load().colors, m);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\n[tui]\n"), "{text}");
        assert!(text.contains("\nsuggest = true\n"), "{text}");
    }
}
