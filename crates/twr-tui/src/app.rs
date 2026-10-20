//! Pure TUI state: what the hub told us (registry, attention stack, the main pane's transcript,
//! recent `decision.made` events) plus client-local view state (the selection, the thread
//! dived into, pins, input line, scroll, stack/grid view and the decisions overlay, decision
//! filters and selection, pending park/resume confirmation, mouse selection, the slash command
//! popup, the action popup, the orchestrator picker). No IO here; the IO loop in `lib.rs` feeds
//! stream items, keys, mouse events and pastes in and executes the [`Action`]s that handling
//! them returns.
//!
//! There are no modes: typing always edits the input line, and the selection (a card, the main
//! pane, a request or a long block in it, a grid tile) is moved with keys that type nothing
//! (Tab, the Shift-arrows). The main pane is the conversation with the orchestrator: a typed
//! line goes to it (`say` with `to: "orchestrator"`), whatever is selected. Only after the user
//! dives into a worker thread (Enter, Enter on its card, a double click, `/open`) does a typed
//! line go to that thread (`thread.send`); Esc or `/back` returns. Typed text never answers a
//! card: the action popup Enter opens on an empty line does ([`ActionPopup`]).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::buffer::Buffer;
use serde_json::Value;
use twr_core::config::CardOrder;
use twr_core::event::{
    is_delete_card, message_ref, AttentionRaised, AttentionReason, AttentionResolved, ChangedBy,
    DecisionProvider, DecisionSite, EventBody, JsonMap, ResolvedBy, ThreadModelChanged, TurnFrom,
    TurnResult, DELETE_KEY, HUB_THREAD, MODEL_RESTART_REASON, MODE_RESTART_REASON,
    RUNTIME_RESTART_REASON,
};
use twr_core::proto::{StreamItem, StreamKind};
use twr_core::reduce::{
    stack_order, AttentionItem, AttentionStack, Registry, ResolveError, ThreadStatus, ThreadView,
};
use twr_core::Event;

use crate::edit::LineInput;
use crate::select::{Anchor, PaneId, Panes, Selection, Target};
use crate::slash::{self, Slash, TrashCmd};

/// The status bar after a middle click: the terminal pastes only on Shift + middle click while
/// the TUI has the mouse.
pub const MIDDLE_CLICK_HINT: &str =
    "paste with Shift+middle click, or turn the mouse off with /mouse off";

/// Runtimes offered for the orchestrator until the hub's list (`orchestrator.show`) arrives.
pub const ORCH_RUNTIMES: &[&str] = &["claude-code", "codex", "grok-build"];

/// Lines scrolled per PgUp/PgDn.
pub const SCROLL_STEP: usize = 10;
/// Lines (or table rows) scrolled per mouse wheel notch.
pub const WHEEL_STEP: usize = 3;
/// Narrowest a grid tile may get; the grid fits `width / TILE_MIN_WIDTH` tiles per row.
pub const TILE_MIN_WIDTH: u16 = 40;
/// `decision.made` events kept for the audit view; older ones are dropped (the control replay
/// may hold many more).
pub const DECISIONS_CAP: usize = 2000;

/// Grid tiles per row at `width` columns (at least one).
pub fn grid_columns(width: u16) -> usize {
    usize::from((width / TILE_MIN_WIDTH).max(1))
}

/// Index reached by moving one row down (or up) from `cur` in a grid of `n` tiles laid out
/// `cols` per row. Moving down into a short last row lands on its last tile; moving past the
/// last row wraps to the same column of the first, and up from the first row wraps to the same
/// column of the last row that has one.
pub fn grid_step_vertical(cur: usize, n: usize, cols: usize, down: bool) -> usize {
    if n == 0 {
        return 0;
    }
    let cols = cols.max(1);
    let cur = cur.min(n - 1);
    let rows = n.div_ceil(cols);
    let (row, col) = (cur / cols, cur % cols);
    if down {
        if row + 1 < rows {
            (cur + cols).min(n - 1)
        } else {
            col
        }
    } else if row > 0 {
        cur - cols
    } else {
        let bottom = (rows - 1) * cols + col;
        if bottom < n {
            bottom
        } else {
            bottom.saturating_sub(cols)
        }
    }
}

/// Which view is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The main pane (the orchestrator conversation, or the thread dived into) and the worker
    /// cards ordered by attention, in a side column (a row above the main pane when narrow).
    #[default]
    Stack,
    /// One tile per worker thread in a wrapping grid; parked threads last.
    Grid,
    /// The decision audit table (`/decisions`): recent `decision.made` events, newest first.
    /// An overlay over the screen it was opened from ([`ViewState::back`]), with keys of its
    /// own ([`AppState::decisions_key`]) and no input box; Esc closes it.
    Decisions,
}

/// A screen of the compact layout (`[tui] compact_width`, `/compact`): on a phone the TUI
/// shows one at a time, under a line naming them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactPane {
    /// The conversation alone (the orchestrator's, or the thread dived into) and the input.
    #[default]
    Chat,
    /// What waits on the user, one line per item; Enter opens one in its action popup.
    Needs,
    /// The threads, one per line.
    Threads,
}

/// A command waiting for a one-key confirmation (`y`) in the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    /// What runs on `y`.
    pub action: Action,
    /// The question shown in the status bar.
    pub prompt: String,
    /// Status shown once confirmed, while the hub works.
    pub pending: String,
    /// A second choice: its key, what it runs and its pending status (the orchestrator
    /// switch's `s`: without the summary).
    pub alt: Option<(char, Action, String)>,
}

/// Client-local view state. Nothing here is an event; pins in particular never leave the client.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewState {
    /// The selected card: a worker thread, [`HUB_THREAD`] for the hub's card, or the current
    /// orchestrator while its attention banner shows (in the stack view's main conversation
    /// only); `None` (as at start, or after Esc) selects nothing. Always something the current
    /// view draws ([`AppState::nav_order`]), and drawn selected while typing goes on. Enter on
    /// an empty line opens its action popup ([`ActionPopup`]), whose option keys answer it
    /// (only what the last frame drew of it, [`AppState::note_drawn`]); it never decides where
    /// a typed line goes.
    pub focus: Option<String>,
    /// The worker thread dived into: the main pane shows it and typed lines go to it. `None`:
    /// the orchestrator conversation.
    pub dive: Option<String>,
    /// The card selected when the dive began: Esc or `/back` selects it again when no card
    /// was selected during the dive ([`AppState::leave_dive`]). `None` outside a dive.
    pub focus_before_dive: Option<String>,
    /// Pinned thread ids (fed to the attention ordering as a tie-breaker).
    pub pins: HashSet<String>,
    /// The input line: what keys that are not about the selection edit, always.
    pub input: LineInput,
    /// The action popup, while it is open ([`AppState::action_popup`]).
    pub action: Option<ActionPopup>,
    /// The card whose attention item the main pane shows as its detail view, scrolled with
    /// PgUp/PgDn ([`AppState::detail_card`]): opened when the action popup had no room for the
    /// item, and kept while that card (or a request of its thread) is the one selected.
    pub detail: Option<String>,
    /// Transcript scroll, in lines up from the bottom as last drawn (0 follows the tail: new
    /// rows show as they come).
    pub scroll: usize,
    /// While scrolled up, the transcript row at the top of the view ([`Anchor`]): it stays
    /// on the same text as rows are added below it, wrapped again, folded or removed, until
    /// the view is scrolled back to the bottom (then it follows again). `None` with `scroll`
    /// set: the view is `scroll` lines up from the bottom (a request just selected).
    pub anchor: Option<Anchor>,
    /// Last error or notice shown in the status bar.
    pub status: Option<String>,
    /// A transient status and when it clears ([`AppState::set_status`]): it goes at that
    /// time, or sooner when the orchestrator's turn starts or ends, or its reply arrives; only
    /// while the status bar still shows it (a newer status is never cleared).
    pub status_until: Option<(String, Instant)>,
    /// The transient status goes only at its time, not when the orchestrator answers: what
    /// taking back a message did ([`AppState::set_timed_status`]), whose interrupt ends the
    /// turn at once.
    pub status_timed: bool,
    /// Stack or grid.
    pub screen: Screen,
    /// Terminal width at the last draw (the grid's columns for the Shift-arrows).
    pub width: u16,
    /// Terminal height at the last draw (how tall the input box may grow,
    /// [`crate::ui::input_cap`]).
    pub height: u16,
    /// A park/resume waiting for `y`.
    pub confirm: Option<Confirm>,
    /// The screen the decisions overlay returns to when it closes.
    pub back: Screen,
    /// Decision table filters.
    pub decision_filter: DecisionFilter,
    /// Selected row of the (filtered, newest first) decision table.
    pub decision_sel: usize,
    /// The selected card's top attention item as the last frame drew it in full, with its
    /// options, and since when ([`AppState::note_drawn`]); `None`: the last frame showed none
    /// (the action popup's answers and Dismiss then act on nothing).
    pub shown: Option<ShownCard>,
    /// A worker request selected in the main conversation (including non-top items).
    pub request: Option<String>,
    /// The help overlay (`/help`, `?` on an empty line) is open: ↑↓, PgUp/PgDn, Home/End and
    /// the wheel scroll it, any other key (Esc) closes it.
    pub help: bool,
    /// How many rows the help overlay is scrolled down.
    pub help_scroll: u16,
    /// The trash overlay (`/trash`) is open with this `trash.list` result; any key closes it.
    pub trash: Option<Value>,
    /// How many rows the trash overlay is scrolled down (↑↓, PgUp/PgDn).
    pub trash_scroll: u16,
    /// Runtimes that can host the orchestrator, as the hub listed them (`orchestrator.show`);
    /// empty until then ([`ORCH_RUNTIMES`] is offered meanwhile).
    pub orch_runtimes: Vec<String>,
    /// The text panes of the last draw, for the mouse ([`crate::ui::render`] returns them).
    pub panes: Panes,
    /// The mouse selection: being dragged, or copied and still highlighted (until the next
    /// key or click).
    pub selection: Option<Selection>,
    /// The TUI has the mouse (`[tui] mouse`, `/mouse`): it selects and copies text itself.
    pub mouse: bool,
    /// Shift-Enter is known to arrive apart from Enter: the keyboard protocol is on
    /// ([`crate::keys`]), or one has come in. The hints name it for a new line only then, and
    /// Ctrl-J (which works everywhere) otherwise.
    pub shift_enter: bool,
    /// The choice of the orchestrator's runtime, while it is pending
    /// ([`AppState::picker_open`]): the first-run one, or `/orch` without arguments.
    pub picker: Option<Picker>,
    /// The orchestrator the hub would start (`orchestrator.show` `runtime`, `model`), for the
    /// main pane's header before an orchestrator thread exists.
    pub orch_default: Option<(String, Option<String>)>,
    /// Whether each orchestrator runtime is installed, as `orchestrator.show` said (`/orch`
    /// suggestions).
    pub orch_installed: Vec<(String, bool)>,
    /// Ctrl-O: open tool-call and bookkeeping groups. Long messages within them have
    /// their own expansion controls in the orchestrator conversation.
    pub expand_all: bool,
    /// Collapsed groups of the main pane opened with a click (their keys; see
    /// [`crate::ui::transcript_view`]).
    pub expanded: HashSet<String>,
    /// Long conversation blocks opened individually, keyed by thread, event and section.
    /// Separate from Ctrl-O's tool/notice groups; kept when visiting another transcript.
    pub long_expanded: HashSet<String>,
    /// The long block selected with Shift-↑/↓ (the main pane selected), Enter, or a click,
    /// while the transcript as last drawn has it ([`AppState::note_panes`]); its expansion is
    /// kept in `long_expanded`.
    pub block: Option<String>,
    /// The queued message selected above the input box, by its ref ([`crate::queue`]): Enter
    /// on an empty line opens its action popup.
    pub queued: Option<i64>,
    /// The selected row of the popup above the input line (`/` commands, `@` mentions).
    pub popup_sel: usize,
    /// The line the popup was closed on (Esc): it stays closed until the line changes.
    pub popup_closed: Option<String>,
    /// The last click on a card or tile, and when: a second one there soon after dives in.
    pub last_click: Option<(String, Instant)>,
    /// Where that last click on a card or tile was: the popup it opened may cover the spot,
    /// and a second click there soon after still dives in.
    pub last_click_at: Option<(u16, u16)>,
    /// Where the left button went down, the control the last frame drew there
    /// ([`crate::select::Target`]) and what that control acts on ([`AppState::press_about`]):
    /// it runs when the button comes up on that same control ([`crate::select::is_tap`]),
    /// still about the same thing. `None` once a press was used up (closing the help).
    pub press: Option<Press>,
    /// The question waiting for `y` as the status bar last drew it with its `yes`, and since
    /// when: a tap on `yes` counts only once it has been on screen for [`CARD_SETTLE`].
    pub confirm_seen: Option<(String, Instant)>,
    /// The working icon moves (`[tui] animate`); else it is the still `_`.
    pub animate: bool,
    /// How the cards are ordered within a group (`[tui] card_order`).
    pub card_order: CardOrder,
    /// With [`CardOrder::Stable`], the cards as last shown ([`crate::order`]), which
    /// [`AppState::settle_order`] brings up to date; `None` before it first did, and with
    /// [`CardOrder::Attention`].
    pub shown_cards: Option<crate::order::Shown>,
    /// Where the mouse pointer last was, and when it moved there: over the cards, it holds
    /// their order ([`AppState::cards_held`]). A terminal that lost focus forgets it.
    pub pointer: Option<(u16, u16, Instant)>,
    /// The working icon's frame for the next draw ([`crate::icon::frame_at`]), set by the IO
    /// loop from its clock.
    pub frame: usize,
    /// The confirm card's detail view: how far it is scrolled and which of its rows have been
    /// on screen ([`CardView`]).
    pub card_view: CardView,
    /// Parked threads are listed among the cards and tiles (`/parked`); else only those
    /// pinned or with an open attention item are ([`AppState::order`]).
    pub show_parked: bool,
    /// Ctrl-B, `/sidebar`: hide the worker cards so the conversation uses the full area.
    pub sidebar_hidden: bool,
    /// `[tui] compact_width`: below it the compact layout is on ([`AppState::compact`]); 0:
    /// never by itself.
    pub compact_width: u16,
    /// Next-message suggestions on (`[tui] suggest`, `/suggest`; [`crate::suggest`]).
    pub suggest: bool,
    /// The model they come from (`[tui] suggest_model`).
    pub suggest_model: String,
    /// `/compact on|off`: the compact layout on or off whatever the width; `None`: by the
    /// width.
    pub compact: Option<bool>,
    /// The compact layout's screen.
    pub compact_pane: CompactPane,
    /// Where the wheel scrolled the cards (the grid's rows of tiles), in any view: kept while
    /// the selection stays the one it was scrolled with, so the selected card may scroll out
    /// of view; selecting another card with a key shows the slice around it again.
    pub card_scroll: Option<CardScroll>,
    /// The selected card before the sidebar was hidden, restored if it is still listed.
    pub sidebar_focus: Option<String>,
    /// Shift-← (or a click in it) selected the main pane (the orchestrator conversation, or
    /// the thread dived into) and Shift-→ has not given the selection back to the cards: in
    /// the stack view, Shift-↑ and Shift-↓ step between its long blocks and requests, no card
    /// is drawn selected and Enter on an empty line opens no card's popup
    /// ([`AppState::main_focused`]). `focus` keeps the card to return to: a request selected
    /// in the conversation meanwhile is the conversation's, not that card's
    /// ([`AppState::selected_request`]).
    pub main_focus: bool,
    /// Ctrl-F, `/focus`: list only working threads and threads needing attention, with
    /// pinned and dived-into threads kept visible.
    pub focus_mode: bool,
    /// Lines sent or run from the input line, oldest first ([`HISTORY_MAX`] at most): Up on
    /// its first row recalls them ([`AppState::history_prev`]).
    pub history: Vec<String>,
    /// The history entry last recalled onto the input line. Browsing goes on only while the
    /// line still shows it ([`AppState::browsing`]): any change to the line, however made
    /// (typing, a paste, a completion), ends it.
    pub history_pos: Option<usize>,
    /// What the input line held when browsing started: Down past the newest entry restores
    /// it.
    pub history_draft: String,
    /// Entries added since the IO loop last saved them.
    pub history_new: Vec<String>,
    /// The next draw repaints the whole screen (Ctrl-L, a cleared conversation): a terminal
    /// that lost track of what is on it shows only what the frame draws.
    pub repaint: bool,
}

/// The status when a key would select a card a narrow dive hides ([`AppState::dive_hides_cards`]).
pub const DIVE_HIDES_CARDS: &str = "the cards are hidden in this dive (Esc goes back to them)";

/// What the card keys say on the compact layout's conversation, which draws no cards.
pub const COMPACT_LISTS: &str = "/n lists what needs you, /t the threads";

/// Most entries the input history keeps.
pub const HISTORY_MAX: usize = 500;

/// What a thread is doing now, as the hub's `thread.activity` last said: its activity line
/// (`activity`, one level more specific than its task), the end of what its agent last said,
/// and its turn's prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadActivity {
    pub activity: String,
    pub said: Option<String>,
    pub task: Option<String>,
}

/// An overlay's scroll keys (the help's, the trash's): ↑↓ a row, PgUp/PgDn ten, Home/End its
/// top and bottom, within `max` (its rows past its height as last drawn). Returns whether
/// `code` is one of them.
fn overlay_scroll(scroll: &mut u16, max: u16, code: KeyCode) -> bool {
    let s = (*scroll).min(max);
    *scroll = match code {
        KeyCode::Up => s.saturating_sub(1),
        KeyCode::Down => s.saturating_add(1).min(max),
        KeyCode::PageUp => s.saturating_sub(10),
        KeyCode::PageDown => s.saturating_add(10).min(max),
        KeyCode::Home => 0,
        KeyCode::End => max,
        _ => return false,
    };
    true
}

/// The cards (or the grid's rows of tiles) as the wheel scrolled them ([`ViewState::card_scroll`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardScroll {
    /// The first card shown (in the grid, the first row of tiles).
    pub start: usize,
    /// The card (tile) shown first then: the list keeps it first when cards come or go above.
    pub first: Option<String>,
    /// The card selected when it was scrolled.
    pub focus: Option<String>,
    /// The list it was scrolled in: a grid row is no card of the column, a card of the
    /// column no card of the row above the conversation.
    pub layout: crate::select::ListLayout,
}

/// A confirm card too long for the orchestrator's banner (or, with no orchestrator, the hub's
/// card), a request too tall for the conversation, or an item the action popup had no room for
/// ([`ViewState::detail`]), which the main pane shows as a scrollable detail view
/// ([`crate::select::CardWindow`]; PgUp/PgDn and the wheel scroll it): how far it is scrolled
/// and which of its rows have been on screen. It is drawn in full (the action popup's answers
/// and Dismiss act on it, it counts as seen) only once every row has been on screen and its
/// options are on screen now. Reset for another card, revision or width, or when its rows
/// stop or start being the item's whole text ([`CardView::complete`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardView {
    /// The card's attention id.
    pub attn_id: String,
    /// Its revision.
    pub revision: u64,
    /// The width its rows were wrapped at.
    pub width: u16,
    /// Rows scrolled from the top (`usize`, as the card's rows: a card may have far more than
    /// 65,535 rows, and every one must be reachable).
    pub scroll: usize,
    /// Each row: it has been on screen.
    pub seen: Vec<bool>,
    /// Its rows are the item's whole text ([`crate::select::CardWindow::complete`]): rows of a
    /// capped preview, or cut at the width, are no proof the item was read, even all seen
    /// (its full text, fetched later under the same revision, was never on screen).
    pub complete: bool,
}

impl CardView {
    /// Whether this is the view of `w`'s card as wrapped by `w`.
    pub fn is_of(&self, w: &crate::select::CardWindow) -> bool {
        self.attn_id == w.attn_id
            && self.revision == w.revision
            && self.width == w.width
            && self.seen.len() == w.rows
            && self.complete == w.complete
    }

    /// Whether every row of item `attn_id`'s `revision` has been on screen, its whole text
    /// ([`CardView::complete`]): read in full.
    pub fn read_in_full(&self, attn_id: &str, revision: u64) -> bool {
        self.complete
            && self.attn_id == attn_id
            && self.revision == revision
            && !self.seen.is_empty()
            && self.seen.iter().all(|s| *s)
    }
}

/// The choice of the agent that runs the orchestrator. The first-run one is offered when no
/// orchestrator was ever chosen (no orchestrator thread, current or retired, and no
/// `[orchestrator] runtime` in the config): choosing records it (`orchestrator.use`), so it is
/// not offered again; Esc leaves it for the next start. `/orch` without arguments opens it to
/// switch the orchestrator (`switch`): choosing asks to confirm the switch, as `/orch
/// <runtime>` does ([`AppState::confirm_switch`]); Esc leaves the orchestrator as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// Runtimes that can host the orchestrator, and whether each is installed (`None`: the
    /// hub did not say).
    pub runtimes: Vec<(String, Option<bool>)>,
    /// The highlighted row.
    pub selected: usize,
    /// The configured default (`[orchestrator] runtime`, else `default_runtime`); for a
    /// switch, the current orchestrator's runtime.
    pub default: String,
    /// The configured model, sent when the default runtime is chosen (never for a switch,
    /// which starts the runtime's default model, as `/orch <runtime>` does).
    pub model: Option<String>,
    /// Opened by `/orch` to switch the orchestrator, not to choose its first one.
    pub switch: bool,
}

impl Picker {
    /// The picker an `orchestrator.show` result asks for, or `None` when an orchestrator was
    /// chosen already (or the hub is too old to say whether the config chose one).
    pub fn from_show(show: &Value) -> Option<Picker> {
        let configured = &show["configured"];
        let first = configured["explicit"].as_bool() == Some(false)
            && show["thread"].is_null()
            && show["retired"].as_u64() == Some(0);
        if !first {
            return None;
        }
        let runtimes: Vec<(String, Option<bool>)> = show["runtimes"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(|r| (r.to_string(), show["installed"][r].as_bool()))
            .collect();
        if runtimes.is_empty() {
            return None;
        }
        let default = configured["runtime"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let selected = runtimes
            .iter()
            .position(|(r, _)| *r == default)
            .unwrap_or(0);
        Some(Picker {
            runtimes,
            selected,
            default,
            model: configured["model"].as_str().map(str::to_string),
            switch: false,
        })
    }
}

/// An attention revision the TUI has only as its capped preview: the hub moved part of it (a
/// long summary, its options) to the blob store and fetching it failed or is still running
/// ([`AppState::incomplete`]). Its preview is drawn, marked as cut short, but it is never
/// drawn in full: no key answers it, and `seen_gseq` stays below its first raise until its
/// full text is fetched and drawn.
#[derive(Debug, Clone)]
pub struct Incomplete {
    /// The revision (the raise's control gseq).
    pub revision: u64,
    /// The raise as it arrived, to fetch its full text again.
    pub raise: StreamItem,
    /// A fetch is running.
    pub fetching: bool,
    /// The last fetch failed.
    pub failed: bool,
    /// Fetches that failed so far.
    pub failures: u32,
    /// When to fetch again after a failure.
    pub retry_at: Option<Instant>,
}

/// Fetches of capped attention text running at once ([`AppState::hydration_due`]).
pub const HYDRATION_PARALLEL: usize = 4;

/// The longest wait before fetching a capped attention text again.
pub const HYDRATION_RETRY_MAX: Duration = Duration::from_secs(60);

/// An attention card on screen: the item and revision an option key answers.
#[derive(Debug, Clone, PartialEq)]
pub struct ShownCard {
    /// Attention id.
    pub attn_id: String,
    /// Its revision.
    pub revision: u64,
    /// When it was first drawn.
    pub since: Instant,
}

/// What the action popup acts on ([`ActionPopup::target`]): what was selected when it opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionTarget {
    /// A card or tile: a worker thread, [`HUB_THREAD`] for the hub's card, or the current
    /// orchestrator for its banner.
    Card(String),
    /// A worker request selected in the main conversation, by attention id.
    Request(String),
    /// A queued message of the main pane's thread, by its ref ([`crate::queue`]).
    Queued(i64),
}

/// What the next Esc does ([`AppState::esc_does`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscDoes {
    /// Closes a popup or an overlay (or cancels a question waiting for `y`).
    Close,
    /// Releases the selection: a long block, a request, the main pane or a card.
    Release,
    /// Leaves the dive, back to the orchestrator conversation.
    Leave,
    /// Nothing.
    Nothing,
}

/// What a row of the action popup does ([`AppState::action_rows`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Answers the item with its option `n` (1-based), as the digit `n` does
    /// ([`AppState::answer`]).
    Answer(usize),
    /// Dives into the thread, to talk to it directly.
    Talk,
    /// Dismisses an item the hub raised ([`AppState::dismiss`]).
    Dismiss,
    /// Pins or unpins the card.
    Pin,
    /// Stops the thread.
    Stop,
    /// Parks the thread (asks `y`).
    Park,
    /// Resumes the parked thread (asks `y`).
    Resume,
    /// Says how to attach to the thread from another terminal (`/attach`).
    Attach,
    /// Takes a queued message back into the input box, to edit and send again.
    Edit,
    /// Takes a queued message back for good (its text goes to the history).
    Delete,
    /// Sends the next queued message now, interrupting the turn that runs.
    SendNow,
}

/// One row of the action popup: what it does and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow {
    pub kind: ActionKind,
    pub label: String,
}

/// The action popup, opened by Enter on an empty line with something selected
/// ([`AppState::open_action`]) and drawn where the `/` popup is, above the input box: the
/// selection's attention item in full (its badge, its summary and every option, numbered) and
/// rows for what can be done (answer with an option, talk to the thread, dismiss, pin, stop,
/// park or resume, attach). Digits answer only here, and only what the popup (or, for an item
/// too long for it, the main pane's detail view) drew in full, once it has been on screen for
/// [`CARD_SETTLE`] since the popup first drew that revision (the popup resets
/// [`ViewState::shown`] as it opens). It follows the selection: Tab, Shift-Tab and the
/// Shift-arrows move it, and the popup shows the newly selected card's; any other change of
/// the selection closes it ([`AppState::sync_action`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionPopup {
    /// What it acts on.
    pub target: ActionTarget,
    /// The highlighted row, by what it does: the rows change under it (the item resolved or
    /// raised again), and a row that went takes the highlight with it rather than passing it
    /// to whatever row takes its place. It opens on Talk to it where there is that row, else
    /// on none: it never opens on an answer, Stop or Park, which Enter would run.
    pub sel: Option<ActionKind>,
    /// Letters typed while it is open: rows whose label does not contain them (case aside)
    /// are dimmed and skipped. Every row stays drawn, so a filter never hides an option.
    pub filter: String,
    /// The item (id and revision) as it had been drawn in full when the popup opened, in its
    /// card or in the main pane's detail view read to its end: an item too long for the popup
    /// is answered from it, its options listed in the popup, only then.
    pub read: Option<(String, u64)>,
    /// The row a tap highlighted, with the item (id and revision) it was tapped on, read long
    /// enough ([`CARD_SETTLE`]): a tap on an answer, Dismiss or Stop runs it only when it is
    /// this one, the item unchanged ([`AppState::tap_row`]). Keys and the wheel arm nothing.
    pub armed: Option<(ActionKind, Option<(String, u64)>)>,
}

/// A press of the left button ([`ViewState::press`]): where, on which control, and what that
/// control acts on ([`AppState::press_about`]).
pub type Press = (u16, u16, Option<Target>, Option<String>);

/// Two clicks on a card at most this far apart dive into its thread.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// How long a card must have been on screen before an option key answers it: a key pressed
/// just after the card changed (the item was replaced, raised again, or another item came on
/// top) was meant for what was there before.
pub const CARD_SETTLE: Duration = Duration::from_millis(600);

/// How long a transient status (what a message to the orchestrator or a worker did) stays in
/// the status bar at most ([`ViewState::status_until`]).
pub const TRANSIENT_STATUS: Duration = Duration::from_millis(2500);

/// How long the second Esc of the double Esc that takes back the last message counts after the
/// first ([`AppState::unsend_esc`]); the input box's title shows the hint meanwhile.
pub const UNSEND_WINDOW: Duration = Duration::from_secs(2);

/// Most recent `turn.started` refs kept to match a message's ref that comes after its turn
/// started ([`AppState::started_refs`]).
const STARTED_REFS: usize = 32;

/// The last message the input line sent (`say` to the orchestrator, `thread.send` in a dive),
/// followed until it can no longer be taken back (`message.withdraw`): Esc twice on an empty
/// line, or `/unsend` ([`AppState::unsend`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sent {
    /// Where it went: the thread dived into, or, once `say` answered, the orchestrator.
    pub thread: Option<String>,
    /// The text as sent.
    pub text: String,
    /// Results still to come of the messages sent up to this one, this one's last: calls are
    /// answered in order, so only the last result is this message's.
    pub waiting: u32,
    /// Its ref, once the hub answered (`ref`).
    pub msg_ref: Option<i64>,
    /// A short command the hub applied at once: nothing was queued, there is no ref.
    pub applied: bool,
    /// The turn it started, once one did (`turn.started` `ext.twr.ref`).
    pub turn: Option<u64>,
    /// That turn called a tool or raised a request: too late.
    pub acted: bool,
    /// That turn ended: too late.
    pub ended: bool,
    /// The hub said it cannot be taken back, and why.
    pub refused: Option<NoUnsend>,
    /// The first Esc on an empty line armed the second, at this time ([`UNSEND_WINDOW`]):
    /// the input box's title shows the hint meanwhile.
    pub armed: Option<Instant>,
    /// The view the armed second Esc counts in: the message's thread's, or, when the first Esc
    /// left the dive the message went to, the orchestrator conversation it went back to. In any
    /// other view (another screen, another thread, even one left since) it takes nothing back.
    pub armed_in: Option<ArmedView>,
    /// The hint ran out unused: for this message Esc goes on to its other steps (`/unsend`
    /// still takes it back).
    pub spent: bool,
    /// A `message.withdraw` for it is on its way.
    pub withdrawing: bool,
    /// A suggestion taken and sent as it was ([`Action::SaySuggested`]).
    pub suggested: bool,
}

/// Where an armed double Esc counts ([`Sent::armed_in`]): the main pane's thread, the screen
/// and the compact layout's pane it was armed in.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmedView {
    /// The main pane's thread.
    pub main: Option<String>,
    /// The screen.
    pub screen: Screen,
    /// The compact layout's pane.
    pub pane: CompactPane,
}

/// A `message.withdraw` on its way ([`AppState::withdrawals`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Withdrawal {
    /// The thread the message went to.
    pub thread: String,
    /// Its ref.
    pub msg_ref: i64,
    /// The line as typed, what goes back in the box (a queued message's preview: its whole
    /// text comes back with the answer).
    pub text: String,
    /// Why it is taken back.
    pub purpose: Retract,
    /// The user sent it: only then may its text go back in the box or the history, as theirs
    /// (a queued message of the orchestrator's, or a `!!` command's output, never does).
    pub mine: bool,
    /// A suggestion sent as it was: back in the box it is still one, and it never goes to
    /// the history.
    pub suggested: bool,
}

/// Why a message is taken back ([`Withdrawal::purpose`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Retract {
    /// The last message sent, with Esc twice or `/unsend`: back in the box, as typed.
    #[default]
    Unsend,
    /// A queued message, to edit it: its whole text back in the box (`↑`, `edit`).
    Edit,
    /// A queued message, to delete it: its text to the history (`delete`).
    Delete,
}

/// Why the last message cannot be taken back ([`AppState::unsend_check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoUnsend {
    /// No message, or none the hub can name yet (no ref), or taken back already.
    Nothing,
    /// A short command the hub applied at once.
    Applied,
    /// Its turn acted on it, or ended.
    TooLate,
    /// Its turn started on a runtime that cannot interrupt one (a pty, a remote box).
    Uninterruptible,
}

/// The status bar when a command about the selected card runs while the main pane is selected
/// ([`AppState::main_focused`]).
pub const MAIN_SELECTED: &str = "the conversation is selected: Shift-→ selects the cards";

/// The status bar when the action popup's answer or Dismiss runs while the selected item is
/// not on screen with its options.
pub const NOT_ON_SCREEN: &str =
    "the selected item is not on screen with its options; select a card you can read";

/// Something the IO loop must do on the hub's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A fresh orchestrator session, without a handoff (`/clear`).
    ResetOrchestrator,
    /// Leave the TUI.
    Quit,
    /// Answer an attention item with one of its options.
    Respond {
        /// Attention id.
        attn_id: String,
        /// The revision of the item the card showed (`AttentionItem::revision`, 0: unknown):
        /// the hub refuses the answer if the item was raised again since (it may ask
        /// something else now).
        revision: u64,
        /// The chosen option, sent as `answer.decision`.
        decision: String,
    },
    /// Send the input line to the orchestrator (`say` with `to: "orchestrator"`: the hub
    /// skips its routing and hands it to the orchestrator).
    Say {
        /// The text typed by the user.
        text: String,
        /// The control gseq on screen when the user pressed Enter (`say` `seen_gseq`): the
        /// orchestrator can relay an answer the user spelled out only to an item raised by
        /// then.
        seen_gseq: u64,
    },
    /// [`Action::Say`] for a suggested message the user took and sent unedited (`say` with
    /// `suggested`): never applied as a short command at once.
    SaySuggested {
        /// The text.
        text: String,
        /// As [`Action::Say`]'s.
        seen_gseq: u64,
    },
    /// Send the input line straight to the thread dived into (`thread.send`).
    Send {
        /// Target thread id.
        thread: String,
        /// The text.
        text: String,
    },
    /// Run the user's `!` command (`shell.run`, [`crate::shell`]).
    ShellRun {
        /// The thread dived into; `None`: the orchestrator's.
        thread: Option<String>,
        /// The command line, without the `!`.
        command: String,
        /// The orchestrator view's folder (`!cd`), when it was moved.
        cwd: Option<String>,
        /// `!!`: its output goes to the thread when it ends.
        share: bool,
        /// Its number among this view's commands: what a stop asked before the answer names.
        launch: u64,
    },
    /// Stop `!` command `launch` once `shell.run` answered it (Ctrl-C before the answer):
    /// sent at once, after the run on the same ordered calls, so it stops the command even if
    /// the view that asked is gone when the answer comes.
    ShellStopLaunch { launch: u64 },
    /// Move the orchestrator view's folder (`!cd`, `shell.cd`).
    ShellCd {
        /// The folder now (`None`: the orchestrator's).
        cwd: Option<String>,
        /// Where to (`None`: the orchestrator's folder).
        dir: Option<String>,
    },
    /// Stop a running `!` command (`job.cancel`): Ctrl-C, or its `■ stop`.
    JobCancel {
        /// Its job.
        job: String,
    },
    /// Run the next queued message now, interrupting the turn that runs
    /// (`message.send_now`).
    SendNow {
        /// The thread it waits for.
        thread: String,
        /// Its ref.
        msg_ref: i64,
    },
    /// Take back a message sent (`message.withdraw`): Esc twice on an empty line, `/unsend`,
    /// or a queued message edited or deleted.
    Withdraw {
        /// The thread it went to.
        thread: String,
        /// Its ref, as `say` / `thread.send` returned it.
        msg_ref: i64,
    },
    /// Set a thread's permission mode (`thread.set_mode`, `/mode`).
    SetMode {
        /// Thread id.
        thread: String,
        /// The mode id, as typed (the hub checks it).
        mode: String,
    },
    /// Set a thread's model and/or reasoning effort, or move it to another runtime
    /// (`thread.set_model`, `/model`).
    SetModel {
        /// Thread id.
        thread: String,
        /// The model, as typed; `None` keeps it (`default` or blank: the runtime's default).
        model: Option<String>,
        /// The reasoning effort, as typed; `None` keeps it (the hub checks it).
        effort: Option<String>,
        /// The runtime, as typed; `None` keeps it (the hub checks it).
        runtime: Option<String>,
    },
    /// Rename a worker thread (`thread.rename`, `/rename`).
    Rename {
        /// Thread id.
        thread: String,
        /// The new name, as typed (the hub checks it).
        name: String,
    },
    /// Ask to delete one or more threads (`thread.delete`, `/delete`): the hub raises one
    /// delete card for all of them that only the user answers; nothing is deleted before.
    Delete {
        /// Thread ids, in the order given, without duplicates.
        threads: Vec<String>,
    },
    /// Show the trash (`trash.list`, `/trash`): its result opens the trash overlay
    /// ([`AppState::apply_result`]).
    TrashList,
    /// Ask the hub which trash items `/trash empty` names (`trash.empty` with `dry_run`): its
    /// result opens the question listing them ([`AppState::apply_result`]).
    TrashChoose {
        /// Items as typed.
        items: Vec<String>,
        /// Every item.
        all: bool,
    },
    /// Purge trash items now (`trash.empty`, after `y` to the question listing them).
    TrashEmpty {
        /// The ids the question listed.
        items: Vec<String>,
    },
    /// Put a deleted thread back from the trash (`trash.restore`, `/trash restore`).
    TrashRestore {
        /// The item as typed.
        item: String,
        /// The name to restore it under.
        as_name: Option<String>,
    },
    /// Dismiss a hub-raised attention item (`attention.dismiss`): a "which thread?" question,
    /// a review, an error or a stalled notice. Never a runtime's permission or question.
    Dismiss {
        /// Attention id.
        attn_id: String,
        /// The revision of the card on screen (0: unknown), as for [`Action::Respond`].
        revision: u64,
    },
    /// Stop a thread.
    Stop {
        /// Thread id.
        thread: String,
    },
    /// Park a thread (`thread.park`).
    Park {
        /// Thread id.
        thread: String,
    },
    /// Resume a parked thread (`thread.resume`).
    Resume {
        /// Thread id.
        thread: String,
    },
    /// Delete the build output of a thread's worktree (`thread.trim`).
    Trim {
        /// Thread id.
        thread: String,
    },
    /// Switch the orchestrator to `runtime` (`orchestrator.use`): a new orchestrator session
    /// that starts with a handoff.
    UseOrchestrator {
        /// Runtime name.
        runtime: String,
        /// Model; `None` = the runtime's default.
        model: Option<String>,
        /// Ask the current orchestrator for its handoff summary first.
        summary: bool,
    },
    /// Start a thread (`thread.new`, `/new`).
    NewThread {
        /// A configured project name or a path.
        project: String,
        /// Runtime; `None` = the project's.
        runtime: Option<String>,
        /// Name; `None` = derived by the hub.
        name: Option<String>,
    },
    /// Put selected text on the clipboard (OSC 52, and a local clipboard command when there
    /// is one).
    Copy {
        /// The text.
        text: String,
    },
    /// Take the mouse (`true`: the TUI selects text) or leave it to the terminal (`/mouse`).
    Mouse(bool),
    /// Switch to workspace `name` (a tab, `/ws`): the IO loop opens its hub (starting it when
    /// needed) and shows it in place of this one.
    Workspace {
        /// The workspace.
        name: String,
        /// Create it first (`/ws` with a new name, once confirmed).
        create: bool,
    },
}

/// One rendered unit of a thread's transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Hub facts visible when tool calls and bookkeeping notices are folded.
    HubUpdate {
        text: String,
        user_message: Option<String>,
        attn_id: Option<String>,
        /// The worker thread it is about (`about_thread`), for its color.
        about: Option<String>,
        /// How the request it raised ended, once a later update said so (`allow`, `deny`,
        /// `cancel`, `closed`, ...): that update is merged into this one.
        outcome: Option<String>,
        /// Its event's seq: what tells it from another update of the same attention id (an
        /// id may be raised again after it ended).
        seq: u64,
        /// It says how a request ended whose raise is not in the transcript: an end of its
        /// own, never taken for a raise.
        resolution: bool,
    },
    /// A turn boundary with the prompt that started it.
    TurnStart {
        /// Event sequence, for persistent block identity.
        seq: u64,
        /// Turn number, if known.
        turn: Option<u64>,
        /// Who sent the prompt.
        from: TurnFrom,
        /// The prompt.
        prompt: String,
        /// A hub notice to the orchestrator (`ext.twr.notice`): updates about the threads,
        /// not a message from the user.
        notice: bool,
        /// The user took the message back while its turn ran (`turn.withdrawn`): drawn struck
        /// out, labelled "taken back".
        withdrawn: bool,
    },
    /// Agent prose. `open` while deltas are still arriving.
    Text {
        /// First delta's event sequence (unchanged by the final replacement).
        seq: u64,
        /// Accumulated text.
        text: String,
        /// Still receiving deltas.
        open: bool,
    },
    /// A tool call, shown as one line; `ok` is filled by the matching tool result.
    Tool {
        /// Call id, used to match the result.
        call_id: String,
        /// Tool name; a twr action is `mcp__twr__<action>` whichever runtime called it
        /// ([`twr_action`]).
        tool: String,
        /// Short input summary (a twr action's own input).
        summary: String,
        /// `None` while pending.
        ok: Option<bool>,
        /// The thread a twr action is about, as its input or its result names it
        /// ([`call_thread`], [`result_thread`]); the main pane names it on a folded group of
        /// calls that are all about one thread.
        thread: Option<String>,
    },
    /// The end of a turn.
    TurnEnd {
        /// How it ended.
        result: TurnResult,
        /// Duration, if reported.
        duration_ms: Option<u64>,
    },
    /// A `!` command the user ran ([`crate::shell`]), from its job's `task.updated` events:
    /// the command, where it ran, how it ended and what it printed.
    Shell {
        /// The seq of the event that started it, for persistent block identity.
        seq: u64,
        /// Its job.
        job: String,
        /// The command line.
        command: String,
        /// The folder it runs in.
        cwd: String,
        /// `!!`: its output goes to the thread when it ends.
        share: bool,
        /// Still running.
        running: bool,
        /// How it ended: `exited`, `cancelled`, `timed_out`, `failed_to_start`, `lost`.
        outcome: Option<String>,
        /// Its exit code.
        exit: Option<i64>,
        /// The signal that killed it.
        signal: Option<i64>,
        /// How long it ran.
        duration_ms: Option<u64>,
        /// What it printed: the end recorded with its end, or the end of its log as last read
        /// (`job.log`), as plain text ([`twr_core::shell::plain_output`]).
        output: String,
        /// Its log was read after it ended (or there is nothing more to read): `output` is
        /// all there is to show.
        read: bool,
    },
    /// A one-line notice (attention raised/resolved, report, stop, crash).
    Notice {
        /// Event sequence, for persistent block identity.
        seq: u64,
        /// Visual weight.
        level: NoticeLevel,
        /// Text.
        text: String,
    },
}

/// Weight of a transcript notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// Neutral information.
    Info,
    /// Needs the user.
    Attention,
    /// Something failed.
    Error,
}

/// Most entries a transcript keeps (the oldest are dropped).
pub const TRANSCRIPT_MAX_ENTRIES: usize = 5000;

/// Most transcripts kept of threads the main pane showed before ([`AppState::transcripts`]).
pub const TRANSCRIPT_CACHE: usize = 8;

/// A thread's transcript (the main pane's), built from its per-thread event stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    /// Thread id this transcript belongs to.
    pub thread: String,
    /// Entries in order (at most [`TRANSCRIPT_MAX_ENTRIES`]).
    pub entries: Vec<Entry>,
    /// Each entry's id, in step with `entries`: given in order, never twice, and kept while
    /// entries before it are removed or text is merged into it (what a scrolled view is
    /// anchored on, [`crate::select::Anchor`]).
    pub ids: Vec<u64>,
    /// The id the next entry gets.
    pub next_id: u64,
    /// Highest seq applied (replays below it are ignored).
    pub last_seq: u64,
    /// The thread's head when its subscription was made: until `last_seq` reaches it, the
    /// events coming in are history replayed, not new ones ([`Transcript::catching_up`]).
    /// `u64::MAX` while the subscription is being made; `None` once there is none to wait for
    /// (it failed, or the transcript was put away).
    pub catch_up: Option<u64>,
    /// While a replay is applied, the entries and their ids as they were when it began: what
    /// the pane shows ([`Transcript::visible`]), so a frame drawn meanwhile (a key, a tick, a
    /// control event) never shows it half applied.
    pub shown: Option<(Vec<Entry>, Vec<u64>)>,
    /// The thread's runtime as the events applied so far name it (`thread.created`, a
    /// `thread.model_changed` that moved it): what a handoff marker names. `None` until one
    /// did (the transcript replays only the latest events).
    pub runtime: Option<String>,
}

impl Transcript {
    /// An empty transcript for `thread`, waiting for its subscription (it reads as catching
    /// up until the replay is applied: the pane says it loads rather than that it is empty).
    pub fn new(thread: impl Into<String>) -> Self {
        Transcript {
            thread: thread.into(),
            catch_up: Some(u64::MAX),
            ..Default::default()
        }
    }

    /// Applies one event of this thread. Events of other threads and already-seen seqs are ignored.
    pub fn apply(&mut self, ev: &Event) {
        if ev.thread != self.thread || (ev.seq != 0 && ev.seq <= self.last_seq) {
            return;
        }
        self.last_seq = self.last_seq.max(ev.seq);
        if !self.catching_up() {
            self.shown = None;
        }
        match &ev.body {
            EventBody::ThreadCreated(p) if !p.runtime.is_empty() => {
                self.runtime = Some(p.runtime.clone());
            }
            EventBody::TurnStarted(p) => {
                self.close_text();
                // The first turn of a thread's new runtime carries a handoff of the previous
                // session (`ext.twr.runtime_handoff`): marked, never shown (the prompt is the
                // message as given).
                let handoff = p
                    .ext
                    .as_ref()
                    .and_then(|m| m.get("twr"))
                    .and_then(|t| t.get(RUNTIME_HANDOFF_KEY))
                    .is_some_and(|h| !h.is_null());
                if handoff {
                    let rt = self
                        .runtime
                        .as_deref()
                        .map(|r| format!("{r} "))
                        .unwrap_or_default();
                    self.notice(
                        NoticeLevel::Info,
                        format!("new {rt}session, with a handoff of the previous one"),
                    );
                }
                let notice = p.from == TurnFrom::Runtime
                    || p.from == TurnFrom::Orchestrator
                        && p.ext
                            .as_ref()
                            .and_then(|m| m.get("twr"))
                            .and_then(|t| t.get("notice"))
                            .and_then(Value::as_bool)
                            == Some(true);
                self.push(Entry::TurnStart {
                    seq: ev.seq,
                    turn: ev.turn,
                    from: p.from,
                    prompt: p.prompt.clone(),
                    notice,
                    withdrawn: false,
                });
            }
            // A message taken back: its prompt stays, struck out.
            EventBody::TurnWithdrawn(_) => {
                let start = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::TurnStart {
                        turn, withdrawn, ..
                    } if *turn == ev.turn => Some(withdrawn),
                    _ => None,
                });
                if let Some(withdrawn) = start {
                    *withdrawn = true;
                }
            }
            EventBody::AgentText(p) => {
                let capped = p
                    .extra
                    .get(twr_core::blob::CAPPED_KEY)
                    .and_then(Value::as_array)
                    .is_some_and(|fields| fields.iter().any(|f| f.as_str() == Some("text")));
                let open = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Text {
                        text, open: true, ..
                    } => Some(text),
                    _ => None,
                });
                match (open, p.is_final) {
                    // The final text carries the whole message: it replaces the deltas.
                    (Some(text), true) => {
                        // A failed blob fetch must never replace complete streamed text
                        // with the final event's 512-byte preview.
                        if !capped || text.len() < p.text.len() {
                            *text = p.text.clone();
                        }
                        self.close_text();
                    }
                    (Some(text), false) => text.push_str(&p.text),
                    (None, is_final) => self.push(Entry::Text {
                        seq: ev.seq,
                        text: p.text.clone(),
                        open: !is_final,
                    }),
                }
                if capped && p.is_final {
                    self.notice(
                        NoticeLevel::Error,
                        "could not load the full message; showing available text".into(),
                    );
                }
            }
            EventBody::AgentToolCall(p) => {
                let (tool, summary) = match twr_action(&p.tool, p.ext.as_ref(), &p.input_summary) {
                    Some((action, input)) => (format!("{TWR_TOOL}{action}"), input),
                    None => (p.tool.clone(), p.input_summary.clone()),
                };
                let thread = call_thread(&tool, &summary);
                self.push(Entry::Tool {
                    call_id: p.call_id.clone(),
                    tool,
                    summary,
                    ok: None,
                    thread,
                })
            }
            EventBody::AgentToolResult(p) => {
                let call = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool {
                        call_id,
                        tool,
                        ok,
                        thread,
                        ..
                    } if *call_id == p.call_id => Some((tool, ok, thread)),
                    _ => None,
                });
                if let Some((tool, ok, thread)) = call {
                    *ok = Some(p.ok);
                    // A successful result names the thread canonically (the input may have
                    // said a prefix, `alp` for `alpha`): it wins over the input's reference.
                    if let Some(action) = tool.strip_prefix(TWR_TOOL) {
                        let named =
                            p.ok.then(|| result_thread(action, &p.output_summary))
                                .flatten();
                        if named.is_some() || thread.is_none() {
                            *thread = named.or_else(|| result_thread(action, &p.output_summary));
                        }
                    }
                }
            }
            EventBody::TurnEnded(p) => {
                self.close_text();
                self.push(Entry::TurnEnd {
                    result: p.result,
                    duration_ms: p.duration_ms,
                });
            }
            // A job's task is the hub's: no other source draws a `!` command.
            EventBody::TaskUpdated(p) if ev.source == twr_core::event::Source::Hub => {
                self.apply_shell(ev.seq, p)
            }
            // What the orchestrator read of a project: shown like a hub update, so it stays on
            // screen when tool calls are folded.
            EventBody::ProjectRead(p) => self.push(Entry::HubUpdate {
                text: p.line(),
                user_message: None,
                attn_id: None,
                about: None,
                outcome: None,
                seq: ev.seq,
                resolution: false,
            }),
            EventBody::ConversationUpdate(p) => {
                let twr = p.ext.as_ref().and_then(|e| e.get("twr"));
                let attn_id = twr
                    .and_then(|e| e.get("attn_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let resolved = twr.and_then(|e| e.get("resolved")) == Some(&Value::Bool(true));
                // A request's end goes onto the update that raised it, when that is here.
                if let (true, Some(id)) = (resolved, &attn_id) {
                    let raised = self.entries.iter_mut().rev().find_map(|e| match e {
                        Entry::HubUpdate {
                            attn_id: Some(a),
                            outcome,
                            resolution: false,
                            ..
                        } if a == id && outcome.is_none() => Some(outcome),
                        _ => None,
                    });
                    if let Some(outcome) = raised {
                        *outcome = Some(request_outcome(&p.text));
                        return;
                    }
                }
                // Raised again (a new revision): the older update of it goes, so no stale
                // `waiting` row stays above the one that will be answered.
                if let (false, Some(id)) = (resolved, &attn_id) {
                    let stale = |e: &Entry| {
                        matches!(e, Entry::HubUpdate {
                            attn_id: Some(a), outcome: None, resolution: false, ..
                        } if a == id)
                    };
                    let mut entries = self.entries.iter();
                    self.ids
                        .retain(|_| entries.next().is_some_and(|e| !stale(e)));
                    self.entries.retain(|e| !stale(e));
                }
                self.push(Entry::HubUpdate {
                    text: p.text.clone(),
                    user_message: p.user_message.clone(),
                    attn_id,
                    about: p.about_thread.clone(),
                    outcome: None,
                    seq: ev.seq,
                    resolution: resolved,
                });
            }
            EventBody::AttentionRaised(p) => self.notice(
                NoticeLevel::Attention,
                format!("{}: {}", reason_notice(p.reason), p.summary),
            ),
            EventBody::AttentionResolved(p) => self.notice(NoticeLevel::Info, resolved_notice(p)),
            EventBody::ReportReady(p) => {
                self.notice(NoticeLevel::Info, format!("report: {}", p.summary))
            }
            EventBody::ThreadStopped(p) => {
                self.close_text();
                // The hub restarts a runtime in new settings (`ext.twr.reason`): not a stop.
                let reason = p
                    .ext
                    .as_ref()
                    .and_then(|m| m.get("twr"))
                    .and_then(|t| t.get("reason"))
                    .and_then(Value::as_str);
                let text = match reason {
                    Some(MODE_RESTART_REASON) => "restarting the runtime in its new mode",
                    Some(MODEL_RESTART_REASON) => "restarting the runtime with its new settings",
                    Some(RUNTIME_RESTART_REASON) => {
                        "the previous runtime stopped; the next turn starts a session of the new one"
                    }
                    Some(twr_core::event::TRIM_RESTART_REASON) => {
                        "the runtime stopped for a trim of its build output; the next prompt \
                         resumes the session"
                    }
                    Some(twr_core::event::BOX_UPGRADE_REASON) => {
                        "restarting on the box's new twr build; the next prompt resumes the \
                         session"
                    }
                    _ => "thread stopped",
                };
                self.notice(NoticeLevel::Info, text.into());
            }
            EventBody::ThreadCrashed(p) => {
                self.close_text();
                let code = p
                    .exit_code
                    .map(|c| format!(" (exit {c})"))
                    .unwrap_or_default();
                self.notice(NoticeLevel::Error, format!("thread crashed{code}"));
            }
            EventBody::ThreadModelChanged(p) => {
                if let Some(r) = &p.runtime {
                    self.runtime = Some(r.clone());
                }
                self.notice(NoticeLevel::Info, model_changed_notice(p))
            }
            EventBody::ThreadRenamed(p) => {
                let by = match p.by {
                    ChangedBy::User => "you",
                    ChangedBy::Orchestrator => "the orchestrator",
                };
                self.notice(
                    NoticeLevel::Info,
                    format!("renamed {} → {} by {by}", p.previous, p.name),
                )
            }
            // By kind, whether or not this build of twr-core has a type for it.
            body if body.kind() == MODE_CHANGED => {
                if let Ok(data) = body.data() {
                    self.notice(NoticeLevel::Info, mode_changed_notice(&data));
                }
            }
            _ => {}
        }
        if self.entries.len() > TRANSCRIPT_MAX_ENTRIES {
            let excess = self.entries.len() - TRANSCRIPT_MAX_ENTRIES;
            self.entries.drain(..excess);
            self.ids.drain(..excess);
        }
    }

    /// A job's `task.updated` (`p`, at `seq`): a `!` command's starts its entry, its end
    /// says how it ended and what it printed; other jobs' and other tasks' show nothing here.
    fn apply_shell(&mut self, seq: u64, p: &twr_core::event::TaskUpdated) {
        let Some(job) = p
            .ext
            .as_ref()
            .and_then(|e| e.get("twr"))
            .and_then(|t| t.get("job"))
        else {
            return;
        };
        let Some(id) = job["id"].as_str() else {
            return;
        };
        let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
        // Its end: what it printed, cut when the hub kept only a preview of it.
        let printed = text(&job["printed"]);
        let cut = job.get("printed_ref").is_some()
            || job
                .get(twr_core::blob::CAPPED_KEY)
                .is_some_and(|c| c.to_string().contains("printed"));
        let catching_up = self.catching_up();
        let at = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Shell { job, .. } if job == id));
        match at {
            Some(i) if !p.running => {
                if let Entry::Shell {
                    running,
                    outcome,
                    exit,
                    signal,
                    duration_ms,
                    output,
                    read,
                    ..
                } = &mut self.entries[i]
                {
                    *running = false;
                    *outcome = job["outcome"].as_str().map(str::to_string);
                    *exit = job["exit"].as_i64();
                    *signal = job["signal"].as_i64();
                    *duration_ms = job["duration_ms"].as_u64();
                    if !*read {
                        *output = printed;
                    }
                    // Seen to its end here: one more read of its log for all of it. Replayed
                    // history shows what its end recorded (`twr job log` has the rest).
                    *read = catching_up || (!cut && *read);
                }
            }
            Some(_) => {}
            None => {
                let Some(shell) = job.get("shell") else {
                    return;
                };
                self.close_text();
                self.push(Entry::Shell {
                    seq,
                    job: id.to_string(),
                    command: text(&shell["command"]),
                    cwd: text(&shell["cwd"]),
                    share: shell["share"].as_bool().unwrap_or(false),
                    running: p.running,
                    outcome: job["outcome"].as_str().map(str::to_string),
                    exit: job["exit"].as_i64(),
                    signal: job["signal"].as_i64(),
                    duration_ms: job["duration_ms"].as_u64(),
                    output: printed,
                    read: !p.running && (!cut || catching_up),
                });
            }
        }
    }

    /// Whether the events coming in are still the replay of what the thread logged before it
    /// was subscribed to: the TUI applies them without drawing each one, so history shows at
    /// once rather than streaming in.
    pub fn catching_up(&self) -> bool {
        self.catch_up.is_some_and(|head| self.last_seq < head)
    }

    /// What the pane shows: the entries and their ids, or while a replay is applied the ones
    /// it began with.
    pub fn visible(&self) -> (&[Entry], &[u64]) {
        match &self.shown {
            Some((entries, ids)) if self.catching_up() => (entries, ids),
            _ => (&self.entries, &self.ids),
        }
    }

    /// A subscription replaying from seq `since` up to the thread's `head`, then live, now
    /// feeds this transcript. What it holds is kept when the replay continues it; otherwise
    /// (events between were never seen, or the log is shorter than what it holds) it starts
    /// over empty. Until the replay is applied, the pane shows what it held before it (the
    /// loading hint when that is nothing, not the "earlier events not shown" notice alone).
    pub fn resume(&mut self, since: u64, head: u64) {
        if self.last_seq < since || self.last_seq > head {
            // (Its ids are never given again: a view anchored on an old entry finds none.)
            let next_id = self.next_id;
            *self = Transcript::new(std::mem::take(&mut self.thread));
            self.next_id = next_id;
        }
        let before = (self.last_seq < head).then(|| (self.entries.clone(), self.ids.clone()));
        self.starts_after(since);
        self.catch_up = Some(head);
        self.shown = before.filter(|_| self.catching_up());
    }

    /// No replay to wait for any more (put away, or its subscription failed or ended): it
    /// reads as caught up and shows its entries.
    pub fn settle(&mut self) {
        self.catch_up = None;
        self.shown = None;
    }

    /// Notes that the stream starts after seq `since` (the earlier events are not shown).
    pub fn starts_after(&mut self, since: u64) {
        if since > 0 && self.entries.is_empty() {
            self.last_seq = self.last_seq.max(since);
            self.notice(
                NoticeLevel::Info,
                format!("earlier events not shown (from seq {})", since + 1),
            );
        }
    }

    fn close_text(&mut self) {
        for e in &mut self.entries {
            if let Entry::Text { open, .. } = e {
                *open = false;
            }
        }
    }

    fn notice(&mut self, level: NoticeLevel, text: String) {
        self.push(Entry::Notice {
            seq: self.last_seq,
            level,
            text,
        });
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.ids.push(self.next_id);
        self.next_id += 1;
    }

    /// The transcript's entries are `entries`, each with an id of its own (tests).
    #[cfg(test)]
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        self.entries.clear();
        self.ids.clear();
        for e in entries {
            self.push(e);
        }
    }
}

/// The prefix of the hub's tools as the orchestrator's runtime names them.
pub const TWR_TOOL: &str = "mcp__twr__";

/// The prefix of the hub's tools as Grok Build names them in a `use_tool` call
/// (`<server>__<tool>`).
const GROK_TWR_TOOL: &str = "twr__";

/// The twr action tool call `tool` runs and the input it gives it (compact JSON): Claude Code
/// and Codex name the call `mcp__twr__<action>`, its summary the input; Grok Build runs it
/// through its `use_tool` (tool kind `other`), whose call names the MCP tool in
/// `ext.<runtime>.mcp_tool` (`twr__<action>`) and leads its summary with that name, then the
/// input. `None` for any other call.
pub fn twr_action(tool: &str, ext: Option<&JsonMap>, summary: &str) -> Option<(String, String)> {
    if let Some(action) = tool.strip_prefix(TWR_TOOL) {
        return Some((action.to_string(), summary.to_string()));
    }
    let mcp_tool = ext?
        .values()
        .find_map(|v| v.get("mcp_tool").and_then(Value::as_str))?;
    let action = mcp_tool.strip_prefix(GROK_TWR_TOOL)?;
    let input = match summary.strip_prefix(mcp_tool)? {
        // No input: as the other runtimes show it.
        "" => "{}",
        rest => rest.strip_prefix(' ')?,
    };
    Some((action.to_string(), input.to_string()))
}

/// Whether twr action `action` is about one thread (whose name its input or result gives):
/// every one but those about all threads, several (`delete_thread`: its result's
/// `thread_name` is only the first of them) or a project.
fn about_one_thread(action: &str) -> bool {
    !matches!(
        action,
        "list_threads" | "subscribe_control" | "read_context" | "set_context" | "delete_thread"
    )
}

/// The string value of `key` in `summary`, a tool call's input or output as compact JSON
/// (`{"mode":"auto","thread":"web"}`), if it holds one whole: a value the summary cuts short
/// (it ends in `...`) or one with escapes is not taken. Text inside other strings never
/// matches, since its quotes are escaped.
pub fn summary_str(summary: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let mut from = 0;
    while let Some(at) = summary[from..].find(&pat) {
        let start = from + at;
        from = start + pat.len();
        // A key, not the end of another string's text.
        if !matches!(summary[..start].chars().last(), Some('{' | ',')) {
            continue;
        }
        let rest = &summary[from..];
        let end = rest.find('"')?;
        let value = &rest[..end];
        return (!value.is_empty() && !value.contains('\\')).then(|| value.to_string());
    }
    None
}

/// The thread a twr action's input names: its `thread`, or the `name` given to
/// `spawn_thread`. `None` for any other tool, an action about no one thread, or when the
/// input names none (or the summary cut it off).
pub fn call_thread(tool: &str, summary: &str) -> Option<String> {
    let action = tool
        .strip_prefix(TWR_TOOL)
        .filter(|a| about_one_thread(a))?;
    summary_str(summary, "thread").or_else(|| {
        (action == "spawn_thread")
            .then(|| summary_str(summary, "name"))
            .flatten()
    })
}

/// The thread the result of twr action `action` names: its `thread_name`, else its
/// `thread`. A successful result's wins over the call's input ([`call_thread`]), which may
/// name the thread by a prefix the TUI does not resolve; a failed one's only fills in a call
/// whose input did not say. `spawn_thread` returns
/// `{thread_name, thread, send?}`, the name first, so even a summary cut short has it (the
/// thread object's own `id` and `name` come too late), and so does every other result about
/// one thread (`set_model`'s `{thread_name, thread, applies, changed, ...}`). `None` for an
/// action about no one thread: the threads in its result (`subscribe_control`'s control
/// items, say) are not what the call is about.
pub fn result_thread(action: &str, summary: &str) -> Option<String> {
    if !about_one_thread(action) {
        return None;
    }
    summary_str(summary, "thread_name").or_else(|| summary_str(summary, "thread"))
}

/// Kind of the event a thread's permission mode change is recorded as.
pub const MODE_CHANGED: &str = "thread.mode_changed";

/// `turn.started` `ext.twr` key of the handoff the first turn of a thread's new runtime
/// delivers (the hub's `hub::switch`).
pub const RUNTIME_HANDOFF_KEY: &str = "runtime_handoff";

/// The transcript notice for `thread.mode_changed` `{mode, previous, by}`: `mode changed from
/// manual to auto (by you)`, or `mode kept at manual (by you)` when the user chose the mode
/// the thread had (`mode == previous`).
pub fn mode_changed_notice(data: &Value) -> String {
    let mode = data["mode"].as_str().unwrap_or("?");
    let from = data["previous"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(|p| format!("from {p} "))
        .unwrap_or_default();
    let by = match data["by"].as_str() {
        Some(b) if b == "user" || b.starts_with("user:") => " (by you)".to_string(),
        Some("orchestrator") => " (by the orchestrator)".to_string(),
        Some("") | None => String::new(),
        Some(other) => format!(" (by {other})"),
    };
    if data["previous"].as_str() == Some(mode) {
        return format!("mode kept at {mode}{by}");
    }
    format!("mode changed {from}to {mode}{by}")
}

/// A model or an effort as the transcript and status bar name it: `None` is `default` (the
/// runtime's own).
fn setting(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("default")
}

/// The transcript notice for `thread.model_changed`: what changed, each from its value before,
/// and who changed it: `model changed from sonnet to claude-fable-5-1, effort from default to
/// high (by the orchestrator)`, or `effort changed from default to high (by you)` when only
/// the effort did. One that changes neither (the hub records none) reads `model kept at
/// sonnet, effort at high`. A move to another runtime names the runtimes and the model and
/// effort the thread has on the new one: `runtime claude-code → codex, model gpt-6.1-sol,
/// effort xhigh (by you)`.
pub fn model_changed_notice(p: &ThreadModelChanged) -> String {
    let by = match p.by {
        ChangedBy::User => " (by you)",
        ChangedBy::Orchestrator => " (by the orchestrator)",
    };
    if let Some(r) = &p.runtime {
        return format!(
            "runtime {} → {r}, model {}, effort {}{by}",
            p.previous_runtime.as_deref().unwrap_or("?"),
            setting(&p.model),
            setting(&p.effort)
        );
    }
    let changes: Vec<String> = [
        ("model", &p.previous_model, &p.model),
        ("effort", &p.previous_effort, &p.effort),
    ]
    .into_iter()
    .filter(|(_, before, now)| before != now)
    .enumerate()
    .map(|(i, (what, before, now))| {
        let verb = if i == 0 { " changed" } else { "" };
        format!("{what}{verb} from {} to {}", setting(before), setting(now))
    })
    .collect();
    if changes.is_empty() {
        return format!(
            "model kept at {}, effort at {}{by}",
            setting(&p.model),
            setting(&p.effort)
        );
    }
    format!("{}{by}", changes.join(", "))
}

/// What `/model <thread>` says of a thread: its model and reasoning effort, and the model its
/// runtime reports when that is another (an alias it resolved, or the one it still runs until
/// it restarts): `api: model fable, effort high (runs claude-fable-5-1)`. A thread that moved
/// to another runtime and has no session of it yet says so: `api: model default, effort
/// default; its first codex session starts with its next turn`.
pub fn model_show(t: &ThreadView) -> String {
    let mut out = format!(
        "{}: model {}, effort {}",
        t.name,
        setting(&t.model),
        setting(&t.effort)
    );
    if let Some(a) = t
        .actual_model
        .as_deref()
        .filter(|a| t.model.as_deref() != Some(a))
    {
        out.push_str(&format!(" (runs {a})"));
    }
    if t.handoff_pending {
        out.push_str(&format!(
            "; its first {} session starts with its next turn",
            t.runtime
        ));
    }
    out
}

/// Short uppercase badge text for an attention reason.
pub fn reason_label(r: AttentionReason) -> &'static str {
    match r {
        AttentionReason::Permission => "PERMISSION",
        AttentionReason::Question => "QUESTION",
        AttentionReason::Error => "ERROR",
        AttentionReason::Review => "REVIEW",
        AttentionReason::Stalled => "STALLED",
    }
}

/// How a transcript notice introduces a raised item: `! error: orchestrator crashed …`.
pub fn reason_notice(r: AttentionReason) -> &'static str {
    match r {
        AttentionReason::Permission => "permission needed",
        AttentionReason::Question => "question",
        AttentionReason::Error => "error",
        AttentionReason::Review => "review",
        AttentionReason::Stalled => "stalled",
    }
}

/// How a request ended, from the hub's update saying so (`<name> request <id> answered:
/// allow.`, `<name> request <id> closed.`): `answered: <the answer>`, or `closed` when nobody
/// answered (an answer that is the word `closed` stays an answer).
pub fn request_outcome(text: &str) -> String {
    // The hub adds one period; the name and the id hold no ` answered: `, the answer may.
    let text = text.strip_suffix('.').unwrap_or(text);
    match text.split_once(" answered: ") {
        Some((_, answer)) => format!("answered: {}", answer.trim()),
        None => "closed".into(),
    }
}

/// The transcript notice for a resolved item: who closed it and how. The hub closes an item
/// nobody answered as `by: timeout`: an error replaced by a newer one (`ext.twr.superseded_by`),
/// a request the agent withdrew or replaced (`ext.<runtime>.cancelled_by`), or one whose turn
/// ended or that otherwise went away.
pub fn resolved_notice(p: &AttentionResolved) -> String {
    let answer = p.answer.as_ref().map(|a| a.decision.as_str());
    let ext = |key: &str| {
        p.ext
            .iter()
            .flat_map(|m| m.values())
            .find_map(|v| v.get(key).cloned())
    };
    match p.by {
        // A dismissal (Dismiss, `attention.dismiss`) is recorded as the answer `cancel`.
        ResolvedBy::User if answer == Some("cancel") => "dismissed by you".into(),
        ResolvedBy::User => match answer {
            Some(a) => format!("answered by you: {a}"),
            None => "closed by you".into(),
        },
        ResolvedBy::Orchestrator => match answer {
            Some(a) => format!("answered by the orchestrator: {a}"),
            None => "closed by the orchestrator".into(),
        },
        ResolvedBy::AutoRetry => "closed: twr retried the turn".into(),
        ResolvedBy::ThreadStopped => "closed: the thread stopped".into(),
        ResolvedBy::Timeout => {
            if p.ext
                .as_ref()
                .and_then(|m| m.get("twr"))
                .is_some_and(|t| t.get("superseded_by").is_some())
            {
                "replaced by a newer error".into()
            } else {
                match ext("cancelled_by").as_ref().and_then(Value::as_str) {
                    Some("superseded") => "replaced by a newer request from the agent".into(),
                    Some(_) => "withdrawn by the agent".into(),
                    None => "closed without an answer".into(),
                }
            }
        }
    }
}

/// One `decision.made` event, as the audit view shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRow {
    /// Event id.
    pub id: String,
    /// Event time, `HH:MM:SS` UTC.
    pub time: String,
    /// Thread id ([`HUB_THREAD`] for hub-level route decisions).
    pub thread: String,
    /// Turn, if the decision belongs to one.
    pub turn: Option<u64>,
    /// Decision site.
    pub site: DecisionSite,
    /// Provider that answered.
    pub provider: DecisionProvider,
    /// Options offered.
    pub options: Vec<String>,
    /// Option chosen.
    pub chosen: String,
    /// Confidence in [0, 1].
    pub confidence: f64,
    /// Time the provider took.
    pub latency_ms: u64,
}

impl DecisionRow {
    /// The row for a `decision.made` event (`None` for other kinds).
    pub fn from_event(ev: &Event) -> Option<Self> {
        let EventBody::DecisionMade(d) = &ev.body else {
            return None;
        };
        Some(DecisionRow {
            id: ev.id.clone(),
            time: ev.ts.format("%H:%M:%S").to_string(),
            thread: ev.thread.clone(),
            turn: ev.turn,
            site: d.site,
            provider: d.provider,
            options: d.options.clone(),
            chosen: d.chosen.clone(),
            confidence: d.confidence,
            latency_ms: d.latency_ms,
        })
    }
}

/// Wire name of a decision site.
pub fn site_label(s: DecisionSite) -> &'static str {
    match s {
        DecisionSite::Route => "route",
        DecisionSite::Grade => "grade",
        DecisionSite::BlockKind => "block_kind",
    }
}

/// Wire name of a decision provider.
pub fn provider_label(p: DecisionProvider) -> &'static str {
    p.as_str()
}

/// Every provider, in display order.
pub const PROVIDERS: [DecisionProvider; 3] = [
    DecisionProvider::Code,
    DecisionProvider::Orchestrator,
    DecisionProvider::User,
];

/// Every site, in display order.
pub const SITES: [DecisionSite; 3] = [
    DecisionSite::Route,
    DecisionSite::Grade,
    DecisionSite::BlockKind,
];

/// Next value of a filter cycling `None` (all) → each of `all` → `None`.
fn cycle<T: Copy + PartialEq>(cur: Option<T>, all: &[T]) -> Option<T> {
    match cur.and_then(|c| all.iter().position(|x| *x == c)) {
        None => all.first().copied(),
        Some(i) => all.get(i + 1).copied(),
    }
}

/// Site and provider filters of the decision table (`None` shows all).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecisionFilter {
    /// Only this site.
    pub site: Option<DecisionSite>,
    /// Only this provider.
    pub provider: Option<DecisionProvider>,
}

impl DecisionFilter {
    /// Whether `row` passes both filters.
    pub fn matches(&self, row: &DecisionRow) -> bool {
        self.site.is_none_or(|s| s == row.site) && self.provider.is_none_or(|p| p == row.provider)
    }

    /// all → route → grade → block_kind → all.
    pub fn cycle_site(&mut self) {
        self.site = cycle(self.site, &SITES);
    }

    /// all → code → orchestrator → user → all.
    pub fn cycle_provider(&mut self) {
        self.provider = cycle(self.provider, &PROVIDERS);
    }
}

/// Counts over the kept decisions for the audit view's summary line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecisionSummary {
    /// Decisions per provider, in [`PROVIDERS`] order.
    pub by_provider: [usize; 3],
    /// Route decisions (the answer each routed message got).
    pub route_settled: usize,
    /// Of those, the ones settled by the orchestrator or the user: the code rules did not
    /// decide.
    pub route_fallback: usize,
}

/// The most recent `decision.made` events, capped at [`DECISIONS_CAP`].
#[derive(Debug, Clone, Default)]
pub struct DecisionLog {
    rows: VecDeque<DecisionRow>,
    seen: u64,
}

impl DecisionLog {
    /// Appends a row, dropping the oldest past [`DECISIONS_CAP`].
    pub fn push(&mut self, row: DecisionRow) {
        self.seen += 1;
        self.rows.push_back(row);
        while self.rows.len() > DECISIONS_CAP {
            self.rows.pop_front();
        }
    }

    /// Rows kept.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether no decision has been kept.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Decisions received in total, including dropped ones.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Kept rows passing `filter`, newest first.
    pub fn newest_first(&self, filter: &DecisionFilter) -> Vec<&DecisionRow> {
        self.rows
            .iter()
            .rev()
            .filter(|r| filter.matches(r))
            .collect()
    }

    /// Summary over every kept row (filters do not apply).
    pub fn summary(&self) -> DecisionSummary {
        let mut s = DecisionSummary::default();
        for r in &self.rows {
            if let Some(i) = PROVIDERS.iter().position(|p| *p == r.provider) {
                s.by_provider[i] += 1;
            }
            if r.site == DecisionSite::Route {
                s.route_settled += 1;
                if matches!(
                    r.provider,
                    DecisionProvider::Orchestrator | DecisionProvider::User
                ) {
                    s.route_fallback += 1;
                }
            }
        }
        s
    }
}

/// A remote box as the hub last said it (`hub.info`'s `boxes`, polled every
/// [`crate::INFO_EVERY`]), with what `runner.connected` / `runner.disconnected` changed since:
/// what a thread's box token says ([`BoxStatus::word`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoxStatus {
    /// `online`, `switching`, `connecting`, `offline`, `needs_install` or `error`.
    pub state: String,
    /// A runner of the box is connected.
    pub online: bool,
    /// The connected runner's twr version.
    pub version: Option<String>,
    /// The build the box's daemon runs (an SSH box's).
    pub running_build: Option<String>,
    /// The box runs another build of the hub's protocol, switched once it is quiet.
    pub switch_pending: bool,
    /// Why the box's last runner was refused, while none is connected.
    pub refused: Option<String>,
    /// The box's runner, or the build the hub would launch there, speaks another runner
    /// protocol than the hub: its refusal or last error says so ([`names_protocol`]). Another
    /// build or version alone is not a mismatch.
    pub protocol_mismatch: bool,
}

/// What a thread's box token adds after `@box` when the box is not simply online
/// ([`BoxStatus::word`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxWord {
    /// The hub is switching the box to another build: its threads wait.
    Switching,
    /// The box runs another build of the same protocol, switched once quiet.
    UpdatePending,
    /// Its runner speaks another runner protocol than the hub.
    Mismatch,
    /// It needs `twr box install`.
    NeedsInstall,
    /// The hub cannot dial it (its box key).
    Error,
    /// The hub has no such box (removed since its threads were made): its threads cannot
    /// run.
    NotConfigured,
    /// The hub is connecting to it.
    Reconnecting,
    /// No runner of it is connected.
    Offline,
}

impl BoxWord {
    /// The word as the token says it.
    pub fn text(self) -> &'static str {
        match self {
            BoxWord::Switching => "switching",
            BoxWord::UpdatePending => "update pending",
            BoxWord::Mismatch => "twr mismatch",
            BoxWord::NeedsInstall => "needs install",
            BoxWord::Error => "error",
            BoxWord::NotConfigured => "not configured",
            BoxWord::Reconnecting => "reconnecting",
            BoxWord::Offline => "offline",
        }
    }
}

impl BoxStatus {
    /// The box as one entry of `hub.info`'s `boxes`.
    fn from_info(b: &Value) -> BoxStatus {
        let text = |v: &Value| v.as_str().map(str::to_string);
        let refused = text(&b["refused"]["reason"]);
        let last_error = text(&b["last_error"]);
        let protocol_mismatch = refused
            .iter()
            .chain(last_error.iter())
            .any(|s| names_protocol(s));
        BoxStatus {
            state: text(&b["state"]).unwrap_or_default(),
            online: b["online"].as_bool() == Some(true),
            version: text(&b["version"]),
            running_build: text(&b["running_build"]),
            switch_pending: b["switch_pending"].as_bool() == Some(true),
            refused,
            protocol_mismatch,
        }
    }

    /// What its token says after `@box`: nothing when it is online with nothing pending;
    /// else, first of these, `switching`, `update pending`, then why it is not online
    /// (`twr mismatch`, `needs install`, `error`, `reconnecting`, `offline`).
    pub fn word(&self) -> Option<BoxWord> {
        if self.state == "switching" {
            return Some(BoxWord::Switching);
        }
        if self.switch_pending {
            return Some(BoxWord::UpdatePending);
        }
        // A state this TUI does not know goes by whether the box is online.
        let known = ["needs_install", "error", "connecting", "offline"];
        if self.state == "online" || (self.online && !known.contains(&self.state.as_str())) {
            return None;
        }
        // `needs install` says what to do, so it wins over the mismatch that caused it (a
        // build of another target that speaks an older protocol).
        if self.state == "needs_install" {
            return Some(BoxWord::NeedsInstall);
        }
        if self.protocol_mismatch {
            return Some(BoxWord::Mismatch);
        }
        Some(match self.state.as_str() {
            "error" => BoxWord::Error,
            "connecting" => BoxWord::Reconnecting,
            _ => BoxWord::Offline,
        })
    }
}

/// Whether `t` runs on a remote box (not the hub's own, `local`): its box token shows.
pub fn on_remote_box(t: &ThreadView) -> bool {
    !t.box_name.is_empty() && t.box_name != "local"
}

/// Whether `s` names a runner protocol (`speaks protocol 6`, `speaking protocol 6`): the hub's
/// words for a runner refused for its protocol, or a build that would be.
fn names_protocol(s: &str) -> bool {
    s.match_indices("protocol ")
        .any(|(i, m)| s[i + m.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

/// Everything the renderer needs.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    /// Name of the connected hub's instance (`hub.info`), independent of the client's env.
    pub instance_name: Option<String>,
    /// Threads, rebuilt from the control stream.
    pub registry: Registry,
    /// Open attention items, rebuilt from the control stream.
    pub attention: AttentionStack,
    /// Highest control `gseq` applied.
    pub last_gseq: u64,
    /// Transcript of the main pane's thread ([`AppState::main_thread`]; `None` before the
    /// orchestrator exists).
    pub transcript: Option<Transcript>,
    /// Transcripts of threads the main pane showed before, the most recent last (at most
    /// [`TRANSCRIPT_CACHE`]): going back to one shows it at once, and its new subscription
    /// replays only what came after it.
    pub transcripts: Vec<Transcript>,
    /// Client-local view state.
    pub view: ViewState,
    /// Control `gseq` the initial replay runs up to (the subscription ack's `head`) while it
    /// is still running. The view kept for this workspace waits for it, and keys do (the IO
    /// loop drops them), so nothing answers a state the screen has not shown yet.
    pub catch_up: Option<u64>,
    /// Recent `decision.made` events for the audit view.
    pub decisions: DecisionLog,
    /// The latest usage of each runtime's account, from `account.usage` (the status bar's
    /// usage segment).
    pub usage: twr_core::usage::UsageView,
    /// How loaded this machine is (where the hub runs its local threads), read every
    /// [`crate::LOAD_EVERY`]: the status bar names what presses while it is busy.
    pub load: Option<twr_core::load::Load>,
    /// The boxes low on disk, with their free bytes (`hub.info`'s `disk`, polled): the status
    /// bar says so.
    pub disk_low: Vec<(String, u64)>,
    /// The hub's remote boxes, by name ([`BoxStatus`]): what a thread's box token says.
    pub boxes: BTreeMap<String, BoxStatus>,
    /// A `hub.info` answer listed the hub's boxes ([`AppState::apply_boxes`]): a remote box
    /// missing from [`AppState::boxes`] since is not configured, not merely undescribed.
    pub boxes_known: bool,
    /// A `hub.info` poll is on its way: the next one waits for it.
    pub info_pending: bool,
    /// How many live `runner.connected` / `runner.disconnected` events were applied: the
    /// order of those against the `hub.info` polls ([`AppState::send_info`]).
    pub live_seq: u64,
    /// Each box's last live runner event, by [`AppState::live_seq`].
    pub box_live: BTreeMap<String, u64>,
    /// [`AppState::live_seq`] when the `hub.info` poll on its way was sent: its answer says
    /// nothing newer of a box than a live event after that.
    pub info_sent: u64,
    /// Attention revisions known only by their capped preview, by attention id
    /// ([`Incomplete`]).
    pub incomplete: BTreeMap<String, Incomplete>,
    /// The open delete cards (`thread.delete`), by attention id, each with the names of the
    /// threads it would delete: its raise says so (`ext.twr.delete.threads`), its attention
    /// item does not.
    pub delete_cards: BTreeMap<String, Vec<String>>,
    /// Attention items in rows from earlier frames ([`crate::ui::WrapCache`]): the renderer
    /// wraps an item's text again only when it changed.
    pub wraps: crate::ui::WrapCache,
    /// What each thread is doing now (`thread.activity`, polled), by thread id.
    pub activity: HashMap<String, ThreadActivity>,
    /// A `thread.activity` call is on its way: the next poll waits for it.
    pub activity_pending: bool,
    /// The workspace shown (`twr ws`), when the TUI can switch workspaces; `None` for a TUI
    /// opened on a socket.
    pub workspace: Option<String>,
    /// The workspaces' tabs, in the strip's order ([`crate::workspace`]).
    pub tabs: Vec<crate::workspace::Tab>,
    /// The workspace being opened, until it is shown or fails.
    pub switching: Option<String>,
    /// What of a view kept for this workspace waits until the replay is applied
    /// ([`AppState::restore_view`]).
    pub restoring: Option<crate::workspace::Restoring>,
    /// How a `twr attach` command reaches the connected hub ([`crate::workspace::attach_via`]):
    /// ` -w <workspace>`, ` --socket <path>` or nothing.
    pub attach_via: String,
    /// The last message sent, while it may still be taken back ([`Sent`]).
    pub sent: Option<Sent>,
    /// The latest turns started from a queued message, newest last: `(thread, ref, turn)`, at
    /// most [`STARTED_REFS`]. A message's ref can come after its turn started.
    pub started_refs: VecDeque<(String, i64, u64)>,
    /// The latest turns that called a tool or raised a request, newest last: `(thread,
    /// turn)`, at most [`STARTED_REFS`]. A message's turn can act before its ref comes.
    pub acted_turns: VecDeque<(String, u64)>,
    /// The `message.withdraw` calls on their way, matched to their answers by ref.
    pub withdrawals: Vec<Withdrawal>,
    /// The main pane's thread's queued messages, as last read ([`crate::queue`]).
    pub queue: Option<crate::queue::QueueView>,
    /// The orchestrator view's folder for `!` commands, once known (`shell.cd`, `!cd`).
    pub shell_cwd: Option<String>,
    /// `shell.cd` was asked for the folder (once).
    pub shell_cwd_asked: bool,
    /// The user's `!cd` is on its way: its answer says where commands run now.
    pub shell_cd_asked: bool,
    /// The `shell.cd` calls on their way, in the order sent (answers come in that order, on
    /// the one connection): `true` for the user's `!cd`, `false` for the TUI asking for the
    /// folder's name.
    pub shell_cds: VecDeque<bool>,
    /// The `!` commands this TUI started that still run, oldest first: Ctrl-C stops the newest.
    pub shell_jobs: Vec<crate::shell::Started>,
    /// The latest `!` commands that ended (their jobs), at most 32: an answer to `shell.run`
    /// that comes after its command's end starts nothing to stop.
    pub shell_done: VecDeque<String>,
    /// The `!` commands sent and not answered yet, in the order sent ([`crate::shell::Launch`]):
    /// Ctrl-C meanwhile stops one as soon as its job is known.
    pub shell_launching: VecDeque<crate::shell::Launch>,
    /// The last `!` command's number ([`Action::ShellRun`]'s `launch`).
    pub shell_launch_seq: u64,
    /// How each `!` command's output is read, by job ([`crate::shell::Reads`]).
    pub shell_reads: HashMap<String, crate::shell::Reads>,
    /// The suggested next message, while it may show ([`crate::suggest`]).
    pub suggestion: Option<crate::suggest::Suggestion>,
    /// A suggestion to ask for: the thread and the turn that ended.
    pub suggest_due: Option<(String, u64)>,
    /// A suggestion call failed already (the status bar said so once).
    pub suggest_failed: bool,
    /// The hub's suggestion counts, as its last answer gave them.
    pub suggest_stats: Value,
    /// Counts the user's sends: a suggestion asked before the last one is not shown.
    pub suggest_epoch: u64,
    /// When the input thread read the terminal event being handled (`None`: now): a key read
    /// before a suggestion appeared never takes it, however late it is handled.
    pub input_at: Option<Instant>,
    /// The suggestion asked for and its epoch ([`AppState::suggest_epoch`]).
    pub suggest_asked: Option<(String, u64, u64)>,
    /// The suggestion last taken into the box: sent unedited, it says so to the hub.
    pub taken_suggestion: Option<String>,
    /// The suggestions sent as they were, `(thread, ref)`, while they may still be queued
    /// (one goes once its turn starts or it is taken back): taken back from the queue, one is
    /// still a suggestion.
    pub suggested_refs: VecDeque<(String, i64)>,
    /// Whether each send still to be answered was a suggestion sent as it was, oldest first
    /// (one connection answers in order): every answer registers its own.
    pub sends_suggested: VecDeque<bool>,
    /// The second Ctrl-C while a `!` command's stop is on its way: the third quits.
    pub shell_quit_armed: bool,
    /// Answers to this workspace's messages (`say`, `thread.send`, `message.withdraw`) that
    /// came while a view kept for it is being restored: applied once it is
    /// ([`AppState::finish_restore`]), into the view as it was left.
    pub deferred: Vec<(&'static str, Value)>,
}

impl AppState {
    /// An empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the initial control replay up to `head` (the `control.subscribe` ack's `head`):
    /// the view kept for this workspace waits until it is applied, and the selection is then
    /// kept to what the replayed stack lists. Nothing is selected until the user selects a
    /// card (the TUI opens with none, unless a kept view had one).
    pub fn begin_catch_up(&mut self, head: u64) {
        if head > self.last_gseq {
            self.catch_up = Some(head);
        } else {
            self.catch_up = None;
            // The view kept for this workspace first: its pins may be what lists its card.
            self.finish_restore();
            self.ensure_focus();
        }
    }

    /// The control stream ended (lost, or the hub closed it): no replay is coming any more,
    /// so a view kept for this workspace is restored with what was applied, and the answers to
    /// its messages that waited for that are applied ([`AppState::finish_restore`]).
    pub fn control_ended(&mut self) {
        self.catch_up = None;
        self.finish_restore();
    }

    /// Applies a pushed stream item: control items update the registry and attention stack,
    /// thread items update the main pane's transcript.
    pub fn apply_event(&mut self, item: &StreamItem) {
        match item.stream {
            StreamKind::Control => {
                if let Some(g) = item.gseq {
                    if g <= self.last_gseq {
                        return;
                    }
                    self.last_gseq = g;
                }
                self.apply_control_at(&item.event, item.gseq);
                self.note_capped(item);
                if self.catch_up.is_some_and(|head| self.last_gseq >= head) {
                    self.catch_up = None;
                    self.finish_restore();
                    self.ensure_focus();
                }
                self.sync_action();
            }
            StreamKind::Thread => {
                self.clear_transient_on(&item.event);
                self.track_sent(&item.event);
                if let Some(t) = &mut self.transcript {
                    t.apply(&item.event);
                }
            }
        }
    }

    /// Records whether control `item` (just applied) raised an attention revision the TUI has
    /// only as its capped preview (an `_capped` marker left: its blob was not fetched), and
    /// forgets the revisions that are no longer current.
    fn note_capped(&mut self, item: &StreamItem) {
        if let EventBody::AttentionRaised(p) = &item.event.body {
            let capped = item
                .event
                .body
                .data()
                .map_or(true, |d| twr_core::blob::has_capped(&d));
            if capped {
                self.incomplete.insert(
                    p.attn_id.clone(),
                    Incomplete {
                        revision: item.gseq.unwrap_or(0),
                        raise: item.clone(),
                        fetching: false,
                        failed: false,
                        failures: 0,
                        retry_at: None,
                    },
                );
            } else {
                self.incomplete.remove(&p.attn_id);
            }
        }
        let attention = &self.attention;
        self.incomplete
            .retain(|id, i| attention.get(id).is_some_and(|a| a.revision == i.revision));
    }

    /// Whether `item`'s current revision is known only by its capped preview
    /// ([`Incomplete`]): drawn marked as cut short, never in full.
    pub fn incomplete(&self, item: &AttentionItem) -> Option<&Incomplete> {
        self.incomplete
            .get(&item.attn_id)
            .filter(|i| i.revision == item.revision)
    }

    /// The capped raises whose full text to fetch now (at most [`HYDRATION_PARALLEL`] running
    /// at once), each with its attention id and revision; they are marked as being fetched.
    pub fn hydration_due(&mut self, now: Instant) -> Vec<(String, u64, StreamItem)> {
        let running = self.incomplete.values().filter(|i| i.fetching).count();
        let mut due = Vec::new();
        for (id, i) in &mut self.incomplete {
            if running + due.len() >= HYDRATION_PARALLEL {
                break;
            }
            if !i.fetching && i.retry_at.is_none_or(|t| t <= now) {
                i.fetching = true;
                due.push((id.clone(), i.revision, i.raise.clone()));
            }
        }
        due
    }

    /// When the next fetch of a capped text is due after a failure, if one waits and can
    /// start then (with [`HYDRATION_PARALLEL`] running, the next one starts when one ends).
    pub fn next_hydration(&self) -> Option<Instant> {
        let running = self.incomplete.values().filter(|i| i.fetching).count();
        if running >= HYDRATION_PARALLEL {
            return None;
        }
        self.incomplete
            .values()
            .filter(|i| !i.fetching)
            .filter_map(|i| i.retry_at)
            .min()
    }

    /// Remembers the threads delete card `p` (raised on `thread`) names, if it is one
    /// ([`AppState::delete_card`]). Called again with the whole raise once a capped one is
    /// restored ([`AppState::hydrated`]): its names may have been in the blob store.
    fn note_delete_card(&mut self, thread: &str, p: &AttentionRaised) {
        if !is_delete_card(thread, &p.ext) {
            return;
        }
        let mut names: Vec<String> = p
            .ext
            .as_ref()
            .and_then(|e| e.get("twr"))
            .and_then(|t| t.get(DELETE_KEY))
            .and_then(|d| d.get("threads"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if names.is_empty() {
            names.push("a thread".into());
        }
        self.delete_cards.insert(p.attn_id.clone(), names);
    }

    /// A fetch of `attn_id`'s capped `revision` finished: `full` is the raise with its text
    /// restored, or why not. Applied only while that revision is current; a result still
    /// capped is a failure, tried again later (doubling, at most [`HYDRATION_RETRY_MAX`]).
    pub fn hydrated(
        &mut self,
        attn_id: &str,
        revision: u64,
        full: Result<StreamItem, String>,
        now: Instant,
    ) {
        let current = self
            .attention
            .get(attn_id)
            .is_some_and(|a| a.revision == revision);
        if !self
            .incomplete
            .get(attn_id)
            .is_some_and(|i| i.revision == revision)
        {
            return;
        }
        if !current {
            self.incomplete.remove(attn_id);
            return;
        }
        let whole = full.ok().filter(|f| {
            matches!(f.event.body, EventBody::AttentionRaised(_))
                && f.event
                    .body
                    .data()
                    .is_ok_and(|d| !twr_core::blob::has_capped(&d))
        });
        match whole {
            Some(f) => {
                self.incomplete.remove(attn_id);
                // The same revision with its full text (`first_revision` and age unchanged).
                self.attention.apply_at(&f.event, Some(revision));
                if let EventBody::AttentionRaised(p) = &f.event.body {
                    self.note_delete_card(&f.event.thread, p);
                }
            }
            None => {
                let Some(i) = self.incomplete.get_mut(attn_id) else {
                    return;
                };
                i.fetching = false;
                i.failed = true;
                i.failures = i.failures.saturating_add(1);
                let wait = Duration::from_secs(1u64 << i.failures.min(6)).min(HYDRATION_RETRY_MAX);
                i.retry_at = Some(now + wait);
            }
        }
    }

    /// Applies one control-stream event whose position is unknown (its attention items get no
    /// revision; see [`AppState::apply_control_at`]).
    pub fn apply_control(&mut self, ev: &Event) {
        self.apply_control_at(ev, None);
    }

    /// Applies one control-stream event at control position `gseq` (the revision an
    /// `attention.raised` gives its item, which an answer to it names).
    pub fn apply_control_at(&mut self, ev: &Event, gseq: Option<u64>) {
        if let Some(row) = DecisionRow::from_event(ev) {
            // A new row lands on top: keep a selection below the top on the same row.
            if self.view.decision_sel > 0 && self.view.decision_filter.matches(&row) {
                self.view.decision_sel = self.view.decision_sel.saturating_add(1);
            }
            self.decisions.push(row);
        }
        self.registry.apply(ev);
        self.attention.apply_at(ev, gseq);
        self.usage.apply(ev);
        self.clear_transient_on(ev);
        self.track_sent(ev);
        match &ev.body {
            EventBody::ThreadCreated(p)
                if p.ext
                    .as_ref()
                    .and_then(|e| e.get("twr"))
                    .and_then(|t| t.get("conversation_reset"))
                    .and_then(Value::as_bool)
                    == Some(true) =>
            {
                if self.view.dive.is_none() {
                    self.view.selection = None;
                }
                if self.catch_up.is_none()
                    && self.view.confirm.as_ref().is_some_and(|c| {
                        matches!(
                            c.action,
                            Action::UseOrchestrator { .. } | Action::ResetOrchestrator
                        )
                    })
                {
                    self.view.confirm = None;
                    self.view.status = Some("cancelled: the orchestrator was reset".into());
                }
                self.transcripts.retain(|t| {
                    !self
                        .registry
                        .get(&t.thread)
                        .is_some_and(|v| v.is_orchestrator())
                });
                if self
                    .transcript
                    .as_ref()
                    .is_some_and(|t| self.registry.get(&t.thread).is_some_and(|v| v.retired))
                {
                    self.transcript = None;
                }
            }
            EventBody::AttentionRaised(p) => self.note_delete_card(&ev.thread, p),
            // A `!` command ended: Ctrl-C quits again.
            EventBody::TaskUpdated(p) if !p.running => {
                if let Some(job) = p.task_id.strip_prefix(twr_core::event::JOB_TASK_PREFIX) {
                    let job = job.to_string();
                    self.shell_ended(&job);
                }
            }
            // A turn ended while the TUI watched: a suggestion may be due; one started: the
            // last one goes.
            EventBody::TurnEnded(_) if self.catch_up.is_none() => {
                self.queue_changed(&ev.thread);
                if let Some(turn) = ev.turn {
                    let thread = ev.thread.clone();
                    self.suggest_after(&thread, turn);
                }
            }
            EventBody::TurnStarted(_) => {
                self.queue_changed(&ev.thread);
                let thread = ev.thread.clone();
                self.suggest_drop(&thread);
            }
            // The thread's queue moved: read it again when it is the main pane's.
            EventBody::TurnEnded(_)
            | EventBody::PromptWithdrawn(_)
            | EventBody::TurnInterrupted(_)
            | EventBody::ThreadStopped(_)
            | EventBody::ThreadCrashed(_) => self.queue_changed(&ev.thread),
            EventBody::ThreadDeleted(p) => self.forget_thread(&ev.thread, p),
            EventBody::RunnerConnected(p) => self.apply_runner(true, p),
            EventBody::RunnerDisconnected(p) => self.apply_runner(false, p),
            EventBody::ThreadRenamed(p) if self.catch_up.is_none() => {
                self.view.status = Some(format!("{} is now {}", p.previous, p.name));
            }
            // The registry brought it back as it was; outside the replay, say so.
            EventBody::ThreadRestored(p) if self.catch_up.is_none() => {
                self.view.status = Some(format!(
                    "{} is back from the trash (parked: /resume {} continues it)",
                    p.name, p.name
                ));
            }
            _ => {}
        }
        let attention = &self.attention;
        self.delete_cards
            .retain(|id, _| attention.get(id).is_some());
        if self.catch_up.is_none() {
            self.ensure_focus();
        }
        // The orchestrator may have been created or switched.
        self.sync_main();
        // Chosen elsewhere (`twr orchestrator use`, or created by a first message): nothing
        // left to choose.
        if self.view.picker.as_ref().is_some_and(|p| !p.switch)
            && self.registry.list().iter().any(|t| t.is_orchestrator())
        {
            self.view.picker = None;
        }
    }

    /// What `t`'s box token says after `@box` ([`BoxStatus::word`]): nothing for a box online
    /// with nothing pending or a local thread; `not configured` for a box the hub's boxes
    /// ([`AppState::boxes_known`]) do not name, and nothing for one before the TUI knows them.
    pub fn box_word(&self, t: &ThreadView) -> Option<BoxWord> {
        if !on_remote_box(t) {
            return None;
        }
        match self.boxes.get(&t.box_name) {
            Some(b) => b.word(),
            None => self.boxes_known.then_some(BoxWord::NotConfigured),
        }
    }

    /// A `hub.info` poll is to be sent, unless one is on its way (`false` then): records when
    /// ([`AppState::info_sent`]), so its answer does not undo a live runner event that comes
    /// after it.
    pub fn send_info(&mut self) -> bool {
        if self.info_pending {
            return false;
        }
        self.info_pending = true;
        self.info_sent = self.live_seq;
        true
    }

    /// Takes the boxes of `hub.info`'s answer `info` ([`AppState::boxes`]); an answer without
    /// them (a failed poll, an older hub) changes nothing. A box whose runner connected or
    /// left after the poll was sent ([`AppState::box_live`]) keeps what that event said when
    /// the answer disagrees on whether it is online (the answer is older), and stays when the
    /// answer does not name it; otherwise the answer's entry replaces it, and a box it does not
    /// name goes.
    pub fn apply_boxes(&mut self, info: &Value) {
        let Some(boxes) = info["boxes"].as_array() else {
            return;
        };
        self.boxes_known = true;
        let sent = self.info_sent;
        let newer = |name: &str| self.box_live.get(name).is_some_and(|s| *s > sent);
        let mut next: BTreeMap<String, BoxStatus> = BTreeMap::new();
        for b in boxes {
            let Some(name) = b["name"].as_str() else {
                continue;
            };
            let said = BoxStatus::from_info(b);
            let entry = match self.boxes.get(name) {
                Some(live) if newer(name) && live.online != said.online => live.clone(),
                _ => said,
            };
            next.insert(name.to_string(), entry);
        }
        for (name, live) in &self.boxes {
            if !next.contains_key(name) && newer(name) {
                next.insert(name.clone(), live.clone());
            }
        }
        self.boxes = next;
    }

    /// A box's runner connected or left (`runner.connected` / `runner.disconnected`), told
    /// before the next `hub.info` poll: the box is online, or offline. A box being switched
    /// stays `switching` while its runner is away; one that comes back is on its new build.
    /// Not during the replay: those are past connections, and `hub.info` says the present.
    fn apply_runner(&mut self, connected: bool, runner: &twr_core::event::RunnerPayload) {
        if self.catch_up.is_some() {
            return;
        }
        self.live_seq += 1;
        self.box_live.insert(runner.box_name.clone(), self.live_seq);
        let b = self.boxes.entry(runner.box_name.clone()).or_default();
        if connected {
            if b.state == "switching" {
                b.switch_pending = false;
            }
            b.state = "online".into();
            b.online = true;
            b.version = runner.version.clone().or(b.version.take());
            b.refused = None;
            b.protocol_mismatch = false;
        } else {
            if b.state != "switching" {
                b.state = "offline".into();
            }
            b.online = false;
        }
    }

    /// Shows `msg` in the status bar; a `transient` one clears after [`TRANSIENT_STATUS`]
    /// from `now` or sooner ([`ViewState::status_until`]).
    pub fn set_status(&mut self, msg: String, transient: bool, now: Instant) {
        self.view.status_until = transient.then(|| (msg.clone(), now + TRANSIENT_STATUS));
        self.view.status_timed = false;
        self.view.status = Some(msg);
    }

    /// Shows `msg` in the status bar for [`TRANSIENT_STATUS`] from `now`, whatever the
    /// orchestrator does meanwhile: what taking back a message did, which the turn its
    /// interrupt ends (or that ends anyway) must not clear before it is read.
    pub fn set_timed_status(&mut self, msg: String, now: Instant) {
        self.set_status(msg, true, now);
        self.view.status_timed = true;
    }

    /// When the transient status clears, while the status bar still shows it.
    pub fn status_deadline(&self) -> Option<Instant> {
        self.view
            .status_until
            .as_ref()
            .filter(|(msg, _)| self.view.status.as_ref() == Some(msg))
            .map(|(_, at)| *at)
    }

    /// Clears the transient status if it is due at `now`.
    pub fn expire_status(&mut self, now: Instant) {
        if self
            .view
            .status_until
            .as_ref()
            .is_some_and(|(_, at)| *at <= now)
        {
            self.clear_transient();
        }
    }

    /// Clears the transient status, if the status bar still shows it.
    fn clear_transient(&mut self) {
        if let Some((msg, _)) = self.view.status_until.take() {
            if self.view.status.as_ref() == Some(&msg) {
                self.view.status = None;
            }
        }
    }

    /// The orchestrator answers: its turn started or ended (control stream), or its reply came
    /// (a final `agent.text` on its thread stream). What the status bar said of the message it
    /// answers is no longer news.
    fn clear_transient_on(&mut self, ev: &Event) {
        if self.view.status_until.is_none() || self.view.status_timed {
            return;
        }
        let answer = match &ev.body {
            EventBody::TurnStarted(_) | EventBody::TurnEnded(_) => true,
            EventBody::AgentText(p) => p.is_final,
            _ => false,
        };
        if answer && self.orchestrator().is_some_and(|o| o.id == ev.thread) {
            self.clear_transient();
        }
    }

    /// Thread `id` (`name`) was deleted (`thread.deleted`): the registry and the attention
    /// stack dropped it already. A dive into it ends (the main pane returns to the orchestrator
    /// conversation), a park, resume or stop of it waiting for `y` is dropped, and so are its
    /// pin and its kept transcript.
    /// Outside the initial replay the status bar says so, with its trash item, and names its
    /// worktree's directory when part of it could not be moved (`worktree_left`: the user
    /// removes it by hand).
    fn forget_thread(&mut self, id: &str, d: &twr_core::event::ThreadDeleted) {
        let name = d.name.as_str();
        if self.view.dive.as_deref() == Some(id) {
            self.view.dive = None;
            self.view.focus_before_dive = None;
        }
        let of_it = |a: &Action| match a {
            Action::Park { thread } | Action::Resume { thread } | Action::Stop { thread } => {
                thread == id
            }
            _ => false,
        };
        if self.view.confirm.as_ref().is_some_and(|c| of_it(&c.action)) {
            self.view.confirm = None;
        }
        self.view.pins.remove(id);
        // Its kept transcript goes too ([`AppState::sync_main`] keeps none of a deleted thread,
        // such as the one on screen when it was deleted).
        self.transcripts.retain(|t| t.thread != id);
        if self.catch_up.is_none() {
            let mut msg = format!("deleted {name}");
            if let Some(item) = &d.trash {
                let short = &item[item.len().saturating_sub(8)..];
                msg.push_str(&format!(
                    "; in the trash as {short} (/trash restore {short} brings it back)"
                ));
            }
            if let Some(dir) = &d.worktree_left {
                msg.push_str(&format!(
                    "; part of {dir} could not be moved (no worktree any more): remove it by hand"
                ));
            }
            self.view.status = Some(msg);
        }
    }

    /// What a hub call returned that the view shows beyond the status bar: a `trash.list`
    /// result opens the trash overlay; a `trash.empty` dry run (`/trash empty`) asks `y`
    /// listing the items it names, and `y` purges exactly those, by id.
    pub fn apply_result(&mut self, method: &'static str, result: &Value) {
        match method {
            "say" | "thread.send" | "message.withdraw" | "shell.run" | "shell.cd"
            | "job.cancel"
                if self.restoring.is_some() =>
            {
                self.deferred.push((method, result.clone()));
            }
            "say" | "thread.send" => self.sent_result(result),
            "message.withdraw" => self.withdraw_result(result),
            "thread.queue" => self.queue_result(result, Instant::now()),
            "message.send_now" => self.send_now_result(result),
            "shell.run" => self.shell_started(result),
            "shell.cd" => self.shell_moved(result),
            "job.log" => self.shell_log(result, Instant::now()),
            "job.cancel" => self.shell_cancel_result(result),
            "suggest.next" => self.suggest_result(result),
            // Only a successful reset requested by this TUI leaves its worker dive.
            // A reset from another client must not change where an unsent draft goes.
            "orchestrator.reset" => {
                self.view.selection = None;
                self.view.repaint = true;
                self.back();
            }
            "trash.list" => {
                // The commands its message quotes run in this view's workspace.
                let mut result = result.clone();
                if let Some(m) = result["message"].as_str() {
                    result["message"] =
                        twr_core::paths::localize_commands(m, &self.attach_via).into();
                }
                self.view.trash = Some(result);
                self.view.trash_scroll = 0;
                self.view.status = None;
            }
            // A failed poll arrives as null: nothing changes but the next poll.
            "hub.info" => {
                self.info_pending = false;
                if let Some(disk) = result["disk"].as_object() {
                    self.disk_low = disk
                        .iter()
                        .filter(|(_, d)| d["low"].as_bool() == Some(true))
                        .filter_map(|(b, d)| Some((b.clone(), d["free"].as_u64()?)))
                        .collect();
                }
                self.apply_boxes(result);
            }
            // A failed call arrives as null: only the next poll is let go.
            "thread.activity" => {
                self.activity_pending = false;
                if let Some(m) = result.as_object() {
                    let text = |v: &Value| v.as_str().map(str::to_string);
                    self.activity = m
                        .iter()
                        .map(|(id, a)| {
                            let activity = ThreadActivity {
                                activity: text(&a["activity"]).unwrap_or_default(),
                                said: text(&a["said"]),
                                task: text(&a["task"]),
                            };
                            (id.clone(), activity)
                        })
                        .collect();
                }
            }
            "trash.empty" if result["dry_run"].as_bool() == Some(true) => {
                self.ask_trash_empty(result)
            }
            _ => {}
        }
    }

    /// Asks `y` before purging the items of `trash.empty` dry run `r`, naming each.
    fn ask_trash_empty(&mut self, r: &Value) {
        let items = r["items"].as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            let message = r["message"]
                .as_str()
                .unwrap_or("the trash is empty: nothing to purge");
            self.view.status = Some(twr_core::paths::localize_commands(
                message,
                &self.attach_via,
            ));
            return;
        }
        // A `y` meant for the question on screen never purges.
        if self.view.confirm.is_some() {
            self.view.status = Some(
                "/trash empty: another question is waiting for y; answer it, then ask again".into(),
            );
            return;
        }
        let ids: Vec<String> = items
            .iter()
            .filter_map(|i| i["item"].as_str().map(str::to_string))
            .collect();
        let named: Vec<String> = items
            .iter()
            .map(|i| {
                format!(
                    "{} ({})",
                    i["short"].as_str().unwrap_or("?"),
                    i["thread_name"].as_str().unwrap_or("?")
                )
            })
            .collect();
        let size = r["size"].as_str().unwrap_or("?");
        let what = match named.as_slice() {
            [one] => format!("{one}, {size}"),
            _ => format!("{} items, {size}: {}", named.len(), named.join(", ")),
        };
        let pending = match ids.len() {
            1 => "purging 1 item: waiting for the hub".to_string(),
            n => format!("purging {n} items: waiting for the hub"),
        };
        self.view.status = None;
        self.view.confirm = Some(Confirm {
            action: Action::TrashEmpty { items: ids },
            prompt: format!(
                "purge {what} for good (no restore after this)? y to confirm, any other key \
                 cancels"
            ),
            pending,
            alt: None,
        });
    }

    /// The threads open delete card `attn_id` would delete, by name; `None` for any other item.
    pub fn delete_card(&self, attn_id: &str) -> Option<&[String]> {
        self.delete_cards.get(attn_id).map(Vec::as_slice)
    }

    /// Whether the first-run orchestrator picker is on screen: pending, the initial replay
    /// applied, and still no orchestrator thread.
    pub fn picker_open(&self) -> bool {
        self.view.picker.is_some() && self.catch_up.is_none()
    }

    /// Card ids in stack order (first = most urgent): [`HUB_THREAD`] first while the hub asks
    /// something (which thread should get a message), then the worker threads. Never an
    /// orchestrator, current or retired: the current one is the main conversation. Never a
    /// parked thread hidden from the listing ([`AppState::hides`]).
    ///
    /// With [`CardOrder::Stable`], the cards keep the order last shown except for what moves
    /// at once ([`crate::order::arrange`]); [`AppState::settle_order`] applies the rest.
    pub fn order(&self) -> Vec<String> {
        let target = self.target_cards();
        let mut order = match self.shown_layout() {
            Some(shown) => crate::order::arrange(&target, shown),
            None => target.into_iter().map(|c| c.id).collect(),
        };
        if self.hub_asks() {
            order.insert(0, HUB_THREAD.to_string());
        }
        order
    }

    /// The worker cards in the order the hub's state asks for ([`stack_order`]), each with
    /// its top item and whether it is parked: what [`AppState::order`] shows, at once with
    /// [`CardOrder::Attention`], in time with [`CardOrder::Stable`].
    fn target_cards(&self) -> Vec<crate::order::Card> {
        stack_order(
            &self.registry,
            &self.attention,
            &self.view.pins,
            self.view.card_order,
        )
        .into_iter()
        .filter_map(|id| {
            let t = self.registry.get(&id)?;
            (!t.is_orchestrator() && !self.hides(t)).then(|| crate::order::Card {
                items: self
                    .card_items(&id)
                    .into_iter()
                    .map(|i| i.attn_id.clone())
                    .collect(),
                parked: t.parked,
                id,
            })
        })
        .collect()
    }

    /// The cards as last shown ([`ViewState::shown_cards`]), with [`CardOrder::Stable`].
    fn shown_layout(&self) -> Option<&crate::order::Layout> {
        self.view
            .shown_cards
            .as_ref()
            .filter(|_| self.view.card_order == CardOrder::Stable)
            .map(|s| &s.layout)
    }

    /// Brings the cards as shown ([`ViewState::shown_cards`]) up to date at `now`
    /// ([`crate::order::settle`]): what moves at once shows, the rest is applied once it has
    /// waited and the cards are not held ([`AppState::cards_held`]). The IO loop calls it
    /// before each draw and at [`AppState::order_deadline`].
    pub fn settle_order(&mut self, now: Instant) {
        if self.view.card_order != CardOrder::Stable {
            self.view.shown_cards = None;
            return;
        }
        // The log is being replayed: its threads are not all there yet.
        if self.catch_up.is_some() {
            return;
        }
        let target = self.target_cards();
        let held = self.cards_held(now);
        self.view.shown_cards = Some(crate::order::settle(
            &target,
            self.view.shown_cards.as_ref(),
            now,
            held,
        ));
    }

    /// When [`AppState::settle_order`] may next have a move to apply: when the waiting one is
    /// due, or, while the mouse holds the cards, when its hold ends; `None` with nothing
    /// waiting, or while a selection or popup holds them (a key or click ends that, and the
    /// loop settles before it draws).
    pub fn order_deadline(&self, now: Instant) -> Option<Instant> {
        let pending = self.view.shown_cards.as_ref()?.pending.as_ref()?;
        if self.cards_held_by_selection() || self.catch_up.is_some() {
            return None;
        }
        if self.hovering(now) {
            return self
                .view
                .pointer
                .map(|(_, _, at)| at + crate::order::HOVER_HOLD);
        }
        Some(pending.due())
    }

    /// Whether the cards are held in their order at `now`: a card or tile is drawn selected,
    /// the action popup or a detail view is open, or the mouse is over the cards.
    pub fn cards_held(&self, now: Instant) -> bool {
        self.cards_held_by_selection() || self.hovering(now)
    }

    fn cards_held_by_selection(&self) -> bool {
        // A card or tile: not the orchestrator's banner, which is selected like one.
        let card = self.view.focus.as_deref().is_some_and(|f| {
            f == HUB_THREAD || self.registry.get(f).is_some_and(|t| !t.is_orchestrator())
        });
        let selected = card
            && self.view.screen != Screen::Decisions
            && !self.main_focused()
            && !self.forced_focus();
        // The detail view as last drawn: an item too long for its card or popup.
        selected || self.view.action.is_some() || self.view.panes.card.is_some()
    }

    /// Whether the mouse is over the cards (as last drawn) at `now`: it moved there less than
    /// [`crate::order::HOVER_HOLD`] ago.
    fn hovering(&self, now: Instant) -> bool {
        self.view.pointer.is_some_and(|(x, y, at)| {
            now < at + crate::order::HOVER_HOLD
                && self
                    .view
                    .panes
                    .list
                    .is_some_and(|l| l.area.contains((x, y).into()))
        })
    }

    /// Whether `id`'s card is as tall as one with an item: it has one, or shows as needing
    /// you until the order is next applied ([`crate::order::Layout::attention`]).
    pub fn card_tall(&self, id: &str) -> bool {
        self.top_attention(id).is_some()
            || self
                .shown_layout()
                .is_some_and(|l| l.attention.contains_key(id))
    }

    /// Whether the card listing leaves out worker `t`: hidden parked threads, or inactive
    /// threads in Focus mode. Pins, the current dive and open attention keep a thread
    /// visible. Hidden threads remain resolvable by commands and `@` mentions.
    pub fn hides(&self, t: &ThreadView) -> bool {
        if self.view.pins.contains(&t.id)
            || self.view.dive.as_deref() == Some(t.id.as_str())
            || self.top_attention(&t.id).is_some()
        {
            return false;
        }
        if t.parked {
            return !self.view.show_parked;
        }
        self.view.focus_mode
            && !matches!(
                t.status(),
                ThreadStatus::Running
                    | ThreadStatus::Background
                    | ThreadStatus::Waiting
                    | ThreadStatus::Error
                    | ThreadStatus::Attached
            )
    }

    /// Names of the parked workers the card listing leaves out ([`AppState::hides`]), in the
    /// registry's order.
    pub fn hidden_parked(&self) -> Vec<&str> {
        self.registry
            .list()
            .into_iter()
            .filter(|t| !t.is_orchestrator() && t.parked && self.hides(t))
            .map(|t| t.name.as_str())
            .collect()
    }

    /// Lists parked threads among the cards (`on`) or hides them (`/parked`); a card that
    /// went is no longer selected.
    fn show_parked(&mut self, on: bool) {
        self.view.show_parked = on;
        let hidden = self.hidden_parked().len();
        self.view.status = Some(if on {
            "parked threads listed (/parked hides them)".to_string()
        } else if hidden == 0 {
            "parked threads hidden: none now (/parked lists them)".to_string()
        } else {
            format!("{hidden} parked hidden (/parked lists them)")
        });
        self.ensure_focus();
    }

    /// Number of non-parked workers hidden by Focus mode.
    pub fn hidden_in_focus(&self) -> usize {
        self.registry
            .list()
            .into_iter()
            .filter(|t| !t.is_orchestrator() && !t.parked && self.hides(t))
            .count()
    }

    fn set_focus_mode(&mut self, on: bool) {
        self.view.focus_mode = on;
        self.view.shown = None;
        self.view.status = Some(if on {
            "Focus mode: working threads and anything needing you (Ctrl-F shows all)".into()
        } else {
            "Focus mode off: all unparked threads listed (Ctrl-F focuses)".into()
        });
        self.ensure_focus();
    }

    /// Sidebar controls always return to the conversation; grid tiles are not a sidebar.
    fn set_sidebar(&mut self, on: bool) {
        if self.view.screen != Screen::Stack {
            self.show(Screen::Stack);
        }
        if on && self.view.sidebar_hidden {
            let focus = self.view.sidebar_focus.take();
            self.set_focus(focus);
        } else if !on && !self.view.sidebar_hidden {
            self.view.sidebar_focus = self.view.focus.clone();
        }
        self.view.sidebar_hidden = !on;
        self.view.shown = None;
        self.view.status = Some(if on {
            "thread sidebar shown (Ctrl-B hides it)".into()
        } else {
            "thread sidebar hidden (Ctrl-B shows it)".into()
        });
        self.ensure_focus();
    }

    /// The current orchestrator's id while it has open attention items: its banner in the
    /// main conversation is selectable like a card (its action popup answers it). The banner
    /// is drawn only in the stack view's main conversation, not in a dive or the grid.
    pub fn orch_banner(&self) -> Option<String> {
        self.orchestrator()
            .filter(|o| self.top_attention(&o.id).is_some())
            .map(|o| o.id.clone())
    }

    /// What can be selected in the stack view: the orchestrator's banner (while it shows: in
    /// the main conversation, never in a dive), then the cards.
    pub fn stack_nav(&self) -> Vec<String> {
        let mut order = if self.view.sidebar_hidden {
            Vec::new()
        } else {
            self.order()
        };
        if let Some(o) = self.orch_banner().filter(|_| self.view.dive.is_none()) {
            order.insert(0, o);
        }
        order
    }

    /// Whether the compact layout is on: `/compact` says so, else the screen is narrower than
    /// `[tui] compact_width`.
    pub fn compact(&self) -> bool {
        self.view.compact.unwrap_or(
            self.view.compact_width > 0
                && self.view.width > 0
                && self.view.width < self.view.compact_width,
        )
    }

    /// The compact layout's threads: the cards in their order (the orchestrator's banner first
    /// while it asks), whatever hides them in the full layout.
    pub fn compact_threads(&self) -> Vec<String> {
        let mut order = self.order();
        if let Some(o) = self.orch_banner().filter(|o| !order.contains(o)) {
            order.insert(0, o);
        }
        order
    }

    /// The cards with something waiting on the user, most urgent (then oldest) first: the
    /// compact layout's "Needs you".
    pub fn needs_order(&self) -> Vec<String> {
        self.compact_threads()
            .into_iter()
            .filter(|id| !self.card_items(id).is_empty())
            .collect()
    }

    /// How many items wait on the user, over [`AppState::needs_order`].
    pub fn needs_count(&self) -> usize {
        self.needs_order()
            .iter()
            .map(|id| self.card_items(id).len())
            .sum()
    }

    /// Whether a compact layout's list shows (not its conversation): its rows are what is
    /// selected, nothing else.
    pub fn compact_listed(&self) -> bool {
        self.compact()
            && self.view.screen != Screen::Decisions
            && self.view.compact_pane != CompactPane::Chat
    }

    /// Shows compact screen `pane`, its list's selection kept when it is in it.
    fn show_pane(&mut self, pane: CompactPane) {
        self.view.compact_pane = pane;
        self.view.action = None;
        self.view.shown = None;
        // The queue is listed in the conversation tab only: a row of it stays selected there.
        if pane != CompactPane::Chat {
            self.view.queued = None;
        }
        // A list's selection is a card: nothing of the conversation stays selected, and its
        // first row is selected when none of its rows is.
        self.fit_compact();
        if self.compact_listed() && self.focused().is_none() {
            let first = self.nav_order().into_iter().next();
            self.set_focus(first);
        }
        self.ensure_focus();
    }

    /// The oldest item waiting on the user, opened on the "Needs you" screen; a note when
    /// nothing waits.
    fn open_oldest_waiting(&mut self) {
        let oldest = self
            .needs_order()
            .into_iter()
            .filter_map(|id| {
                let at = self.card_items(&id).iter().map(|i| i.raised_at).min()?;
                Some((at, id))
            })
            .min_by_key(|(at, _)| *at)
            .map(|(_, id)| id);
        let Some(first) = oldest else {
            self.view.status = Some("nothing waits on you".into());
            return;
        };
        self.view.compact_pane = CompactPane::Needs;
        self.view.main_focus = false;
        self.view.request = None;
        self.view.block = None;
        self.view.queued = None;
        self.set_focus(Some(first));
        if let Some(target) = self.selection_target() {
            self.open_action(target);
        }
    }

    /// The thread the main pane shows: the one dived into, else the current orchestrator.
    pub fn main_thread(&self) -> Option<String> {
        self.view
            .dive
            .clone()
            .or_else(|| self.orchestrator().map(|o| o.id.clone()))
    }

    /// The thread dived into, if any.
    pub fn dived(&self) -> Option<&ThreadView> {
        self.view
            .dive
            .as_deref()
            .and_then(|id| self.registry.get(id))
    }

    /// Keeps the transcript on the main pane's thread (scrolled to the bottom, bookkeeping
    /// groups folded when it changed, long-block choices kept): the one kept from its last time there
    /// ([`AppState::transcripts`]), else a new one. The transcript it leaves is kept, unless
    /// its thread was deleted.
    pub fn sync_main(&mut self) {
        let main = self.main_thread();
        if self.transcript.as_ref().map(|t| &t.thread) == main.as_ref() {
            return;
        }
        // Another thread on the main pane: a double Esc armed for the one before is for no
        // view any more, even once that one shows again.
        if let Some(s) = self.sent.as_mut() {
            s.armed = None;
        }
        self.follow();
        self.view.expanded.clear();
        self.view.block = None;
        let shown = main.map(
            |id| match self.transcripts.iter().position(|t| t.thread == id) {
                Some(i) => self.transcripts.remove(i),
                None => Transcript::new(id),
            },
        );
        if let Some(mut old) = std::mem::replace(&mut self.transcript, shown)
            .filter(|t| !self.registry.is_deleted(&t.thread))
        {
            old.settle();
            self.transcripts.push(old);
            if self.transcripts.len() > TRANSCRIPT_CACHE {
                self.transcripts.remove(0);
            }
        }
    }

    /// A subscription for `thread`'s transcript is being made: the main pane's transcript, when
    /// it is `thread`'s, waits for it (it reads as loading while it is empty, kept from an
    /// earlier visit or not). Returns the seq the transcript ends at, which the subscription
    /// starts after (0 for none).
    pub fn transcript_subscribing(&mut self, thread: &str) -> u64 {
        match self.transcript.as_mut().filter(|t| t.thread == thread) {
            Some(t) => {
                t.catch_up = Some(u64::MAX);
                t.last_seq
            }
            None => 0,
        }
    }

    /// The main pane's transcript has no subscription to wait for (it failed or ended): it no
    /// longer reads as loading.
    pub fn transcript_stalled(&mut self) {
        if let Some(t) = &mut self.transcript {
            t.settle();
        }
    }

    /// Whether the main pane's transcript is still applying its subscription's replay
    /// ([`Transcript::catching_up`]), or waiting for the subscription.
    pub fn transcript_catching_up(&self) -> bool {
        self.transcript
            .as_ref()
            .is_some_and(Transcript::catching_up)
    }

    /// Dives into worker thread `thread`: the main pane shows it and typed lines go to it. Its
    /// card is drawn as where text goes, and nothing is selected: the card selected before is
    /// kept to select again when the dive ends ([`AppState::leave_dive`]). The orchestrator is
    /// the main conversation already: asking to dive into it returns there.
    pub fn dive(&mut self, thread: &str) {
        self.view.request = None;
        let Some(t) = self.registry.get(thread) else {
            self.view.status = Some(format!("no thread {thread}"));
            return;
        };
        if t.is_orchestrator() {
            self.back();
            self.view.status = Some("the orchestrator is the main conversation".into());
            return;
        }
        let id = t.id.clone();
        self.view.status = None;
        self.view.screen = Screen::Stack;
        // From one thread to another: the card to select again stays the one from before the
        // first dive.
        if self.view.dive.is_none() {
            self.view.focus_before_dive = self.view.focus.clone();
        }
        self.view.dive = Some(id);
        self.view.main_focus = false;
        // The compact layout shows a dive on its conversation screen.
        if self.compact() {
            self.view.compact_pane = CompactPane::Chat;
        }
        self.set_focus(None);
        self.sync_main();
        self.ensure_focus();
    }

    /// Back to the orchestrator conversation (also a dive the hub ended: a reset, a delete).
    /// The selection a narrow dive forced on its own card ([`AppState::forced_focus`]) goes.
    pub fn back(&mut self) {
        if self.forced_focus() {
            self.set_focus(None);
        }
        self.view.dive = None;
        self.view.focus_before_dive = None;
        self.view.screen = Screen::Stack;
        if self.compact() {
            self.view.compact_pane = CompactPane::Chat;
        }
        self.sync_main();
        self.ensure_focus();
    }

    /// The user leaves the dive (Esc, `/back`): back to the orchestrator conversation, with
    /// the card selected when the dive began selected again, unless another card was selected
    /// during the dive (or that card is gone).
    fn leave_dive(&mut self) {
        let untouched = self.view.focus.is_none() || self.forced_focus();
        let before = self
            .view
            .focus_before_dive
            .take()
            .filter(|_| self.view.dive.is_some() && untouched);
        self.back();
        if let Some(before) = before {
            if self.view.sidebar_hidden {
                // Checked against the cards when the sidebar shows them again.
                self.view.sidebar_focus = Some(before);
            } else if self.nav_order().contains(&before) {
                self.set_focus(Some(before));
            }
        }
    }

    /// Whether the selection is the one a narrow dive puts on its own card while the cards
    /// are hidden ([`AppState::note_panes`]), not one the user made: Esc does not release it,
    /// it leaves the dive.
    pub fn forced_focus(&self) -> bool {
        self.dive_hides_cards() && self.view.focus == self.view.dive
    }

    /// Tile ids in grid order: [`AppState::order`] with parked threads moved (in order) to the
    /// end, so the hub's card stays first.
    pub fn grid_order(&self) -> Vec<String> {
        // Parked as shown: a thread just parked keeps its tile's place until the move is
        // applied, like its card's.
        let shown = self.shown_layout();
        let parked_now = |id: &String| match shown.filter(|l| l.order.contains(id)) {
            Some(l) => l.parked.contains(id),
            None => self.registry.get(id).is_some_and(|t| t.parked),
        };
        let (parked, mut live): (Vec<String>, Vec<String>) =
            self.order().into_iter().partition(parked_now);
        live.extend(parked);
        live
    }

    /// Selection order of the current screen: what it draws that can be selected. The
    /// decisions screen draws no card: nothing.
    pub fn nav_order(&self) -> Vec<String> {
        // The compact layout's lists are what its keys move through.
        if self.compact() && self.view.screen != Screen::Decisions {
            match self.view.compact_pane {
                CompactPane::Needs => return self.needs_order(),
                CompactPane::Threads => return self.compact_threads(),
                CompactPane::Chat => {}
            }
        }
        match self.view.screen {
            Screen::Stack => self.stack_nav(),
            Screen::Grid => self.grid_order(),
            Screen::Decisions => Vec::new(),
        }
    }

    /// Decision table rows under the current filters, newest first.
    pub fn decision_rows(&self) -> Vec<&DecisionRow> {
        self.decisions.newest_first(&self.view.decision_filter)
    }

    /// Selected decision row index, clamped to the filtered rows (0 when there are none).
    pub fn decision_selected(&self) -> usize {
        let n = self.decision_rows().len();
        self.view.decision_sel.min(n.saturating_sub(1))
    }

    /// Moves the decision selection by `delta` rows, clamped to the table.
    fn move_decision(&mut self, delta: isize) {
        let n = self.decision_rows().len();
        let cur = self.decision_selected() as isize;
        self.view.decision_sel = (cur + delta).clamp(0, n.saturating_sub(1) as isize) as usize;
    }

    /// Opens the selected decision's thread in the stack view: dives into a worker, returns
    /// to the conversation for the orchestrator (hub decisions have no thread).
    fn open_decision(&mut self) {
        let sel = self.decision_selected();
        let Some(thread) = self.decision_rows().get(sel).map(|r| r.thread.clone()) else {
            self.view.status = Some("no decision selected".into());
            return;
        };
        if thread == HUB_THREAD {
            self.view.status = Some("hub decision: no thread to open".into());
            return;
        }
        if self.registry.get(&thread).is_none() {
            self.view.status = Some(format!("thread {thread} is gone"));
            return;
        }
        self.dive(&thread);
    }

    /// `/decisions`: opens the decisions overlay over the current screen, or closes it, back
    /// to the screen it was opened from. What that screen had selected, and a dive, stay as
    /// they were.
    fn toggle_decisions(&mut self) {
        self.view.action = None;
        self.view.card_scroll = None;
        if self.view.screen == Screen::Decisions {
            self.view.screen = self.view.back;
        } else {
            self.view.back = self.view.screen;
            self.view.screen = Screen::Decisions;
        }
        self.ensure_focus();
    }

    /// Leaving the stack view ends a dive: typed lines go to the orchestrator again.
    fn end_dive(&mut self) {
        if self.forced_focus() {
            self.set_focus(None);
        }
        self.view.focus_before_dive = None;
        if self.view.dive.take().is_some() {
            self.sync_main();
        }
    }

    /// `/grid`, Ctrl-G: the grid, or the stack again. The compact layout has no grid.
    fn toggle_grid(&mut self) {
        if self.compact() {
            self.view.status = Some("the compact layout has no grid (/compact off)".into());
            return;
        }
        let grid = self.view.screen == Screen::Grid;
        self.show(if grid { Screen::Stack } else { Screen::Grid });
    }

    /// The selection fits what the compact layout draws, when it came on (by `/compact` or
    /// a narrower screen) or since: it has no grid (one shown is the stack view again), and a
    /// list's selection is one of its rows (or none, as Esc leaves it), nothing of the
    /// conversation.
    fn fit_compact(&mut self) {
        if self.compact() && self.view.screen == Screen::Grid {
            self.view.screen = Screen::Stack;
            self.view.main_focus = false;
            self.ensure_focus();
        }
        if self.compact_listed() {
            self.view.main_focus = false;
            self.view.request = None;
            self.view.block = None;
            let order = self.nav_order();
            if self
                .focused()
                .is_some_and(|f| !order.iter().any(|id| id == f))
            {
                self.set_focus(order.first().cloned());
            }
        }
    }

    /// Switches to `screen` (the stack or the grid), ending a dive; a card the new screen does
    /// not draw is no longer selected.
    fn show(&mut self, screen: Screen) {
        self.view.request = None;
        self.view.main_focus = false;
        self.view.card_scroll = None;
        self.end_dive();
        self.view.screen = screen;
        self.ensure_focus();
    }

    /// Whether the hub's card has open items (questions from `say`; a confirm card shows in
    /// the orchestrator's banner instead, see [`AppState::card_items`]).
    pub fn hub_asks(&self) -> bool {
        !self.card_items(HUB_THREAD).is_empty()
    }

    /// The open items card `card` shows, most urgent first. The current orchestrator's banner
    /// shows the hub's confirm cards ([`AttentionItem::proposal`]: what the orchestrator
    /// proposed from the user's message, for the user to confirm) first, then the
    /// orchestrator's own items; the hub's card ([`HUB_THREAD`]) shows the hub's other items
    /// (its confirm cards too while there is no orchestrator to show them); any other card its
    /// thread's items.
    pub fn card_items(&self, card: &str) -> Vec<&AttentionItem> {
        let pins = &self.view.pins;
        let banner = self.orchestrator().map(|o| o.id.as_str());
        if banner == Some(card) {
            let mut items: Vec<&AttentionItem> = self
                .attention
                .for_thread(HUB_THREAD, pins)
                .into_iter()
                .filter(|i| i.proposal)
                .collect();
            items.extend(self.attention.for_thread(card, pins));
            return items;
        }
        let mut items = self.attention.for_thread(card, pins);
        if card == HUB_THREAD && banner.is_some() {
            items.retain(|i| !i.proposal);
        }
        items
    }

    /// The selected card's id.
    pub fn focused(&self) -> Option<&str> {
        self.view.focus.as_deref()
    }

    /// The selected card's thread.
    pub fn focused_thread(&self) -> Option<&ThreadView> {
        self.focused().and_then(|id| self.registry.get(id))
    }

    /// Worker permissions and questions shown directly in the main conversation.
    pub fn inline_requests(&self) -> Vec<&AttentionItem> {
        self.attention
            .ordered(&self.view.pins)
            .into_iter()
            .filter(|i| {
                matches!(
                    i.reason,
                    AttentionReason::Permission | AttentionReason::Question
                ) && self
                    .registry
                    .get(&i.thread)
                    .is_some_and(|t| !t.is_orchestrator())
            })
            .collect()
    }

    /// The worker request selected in the main conversation (Shift-↑/↓ with the main pane
    /// selected, a click), while it is open. With the sidebar shown its thread's card is
    /// selected with it. With the sidebar hidden its thread has no card: the request, drawn in
    /// the conversation, is what the action popup opens on and answers (after the same drawn /
    /// settled / revision / hydration guards as a card), while commands about a thread that
    /// take the selected one act on the selected card, if any (the orchestrator's), never on
    /// the request's thread.
    pub fn selected_request(&self) -> Option<&AttentionItem> {
        if self.view.screen != Screen::Stack || self.view.dive.is_some() {
            return None;
        }
        let id = self.view.request.as_deref()?;
        self.attention.get(id).filter(|i| {
            matches!(
                i.reason,
                AttentionReason::Permission | AttentionReason::Question
            ) && (self.view.sidebar_hidden
                || self.view.main_focus
                || self.focused() == Some(i.thread.as_str()))
                && self
                    .registry
                    .get(&i.thread)
                    .is_some_and(|t| !t.is_orchestrator())
        })
    }

    /// The card whose item the main pane shows as its detail view ([`ViewState::detail`]):
    /// only in the stack view, while that card (or a request of its thread) is the one
    /// selected and has an item open.
    pub fn detail_card(&self) -> Option<&str> {
        let card = self.view.detail.as_deref()?;
        if self.view.screen != Screen::Stack {
            return None;
        }
        let selected = match self.selected_request() {
            Some(i) => i.thread.as_str(),
            None if self.view.request.is_none() && !self.main_focused() => self.selected_card()?,
            None => return None,
        };
        (selected == card && self.selected_attention().is_some()).then_some(card)
    }

    /// The item the selection's action popup answers: the request selected in the
    /// conversation, else the top item of the card it is about ([`AppState::selected_card`]).
    pub fn selected_attention(&self) -> Option<&AttentionItem> {
        // A queued message selected has no item: its popup never answers the card's.
        if self.view.queued.is_some() {
            return None;
        }
        // A resolved inline selection must not silently become another item's answer.
        if self.view.request.is_some() {
            self.selected_request()
        } else {
            self.selected_card().and_then(|t| self.top_attention(t))
        }
    }

    fn select_request(&mut self, id: &str) {
        let Some(item) = self.inline_requests().into_iter().find(|i| i.attn_id == id) else {
            return;
        };
        let thread = item.thread.clone();
        self.view.queued = None;
        // With the sidebar hidden its thread has no card to select: the selection stays what
        // it was (none, or the orchestrator's card), see [`AppState::selected_request`]. So
        // with the conversation selected: the card it keeps is the one Shift-→ returns to.
        if !self.view.sidebar_hidden && !self.main_focused() {
            self.set_focus(Some(thread));
        }
        self.view.request = Some(id.into());
        self.view.block = None;
        self.view.shown = None;
        self.view.scroll = crate::ui::request_scroll(self, id);
        self.view.anchor = None;
    }

    /// The current orchestrator thread's view (none retired), if one exists.
    pub fn orchestrator(&self) -> Option<&ThreadView> {
        self.registry
            .list()
            .into_iter()
            .find(|t| t.is_current_orchestrator())
    }

    /// Records what the frame just drawn ([`ViewState::panes`], as [`crate::ui::render`]
    /// reported it) showed of the selected card at `now`: its top attention item (item and
    /// revision) when the frame drew it in full with its options ([`crate::select::DrawnItem`]),
    /// with the time it first appeared; else nothing. The IO loop calls it after every draw,
    /// so the action popup's answers and Dismiss act only on an item that is on screen, and
    /// know how long it has been there ([`CARD_SETTLE`]). The popup itself draws its item in
    /// full ([`ActionPopup`]); what it covers is not drawn. An item that is not drawn (the
    /// orchestrator's banner in a dive or the grid, a card scrolled away, a card too short for
    /// its options, anything under an overlay) is never answered.
    pub fn note_drawn(&mut self, now: Instant) {
        // The suggested message on screen: since when (Tab or → takes it only once read).
        if self.shown_suggestion().is_some() {
            if let Some(s) = self.suggestion.as_mut().filter(|s| s.shown.is_none()) {
                s.shown = Some(now);
            }
        }
        // The question waiting for `y`, as drawn with its `yes`: since when it has been.
        let yes = self
            .view
            .confirm
            .as_ref()
            .filter(|_| {
                self.view
                    .panes
                    .targets
                    .iter()
                    .any(|(_, t)| *t == Target::Yes)
            })
            .map(|c| c.prompt.clone());
        self.view.confirm_seen = match (yes, self.view.confirm_seen.take()) {
            (Some(p), Some((q, since))) if p == q => Some((q, since)),
            (Some(p), _) => Some((p, now)),
            (None, _) => None,
        };
        // The rows of a confirm card's detail view this frame showed have been on screen.
        if let Some(w) = self.view.panes.card.clone() {
            let cv = &mut self.view.card_view;
            if !cv.is_of(&w) {
                let same = cv.attn_id == w.attn_id && cv.revision == w.revision;
                *cv = CardView {
                    scroll: if same { cv.scroll } else { 0 },
                    attn_id: w.attn_id.clone(),
                    revision: w.revision,
                    width: w.width,
                    seen: vec![false; w.rows],
                    complete: w.complete,
                };
            }
            for seen in cv.seen.iter_mut().take(w.end).skip(w.start) {
                *seen = true;
            }
        }
        // Never an item known only by its capped preview ([`Incomplete`]).
        let top = self
            .selected_attention()
            .and_then(|selected| {
                self.view
                    .panes
                    .items
                    .iter()
                    .find(|i| i.attn_id == selected.attn_id && i.revision == selected.revision)
            })
            .filter(|i| {
                self.attention
                    .get(&i.attn_id)
                    .is_none_or(|a| self.incomplete(a).is_none())
            })
            .map(|i| (i.attn_id.clone(), i.revision));
        self.view.shown = match (top, self.view.shown.take()) {
            (Some((id, rev)), Some(s)) if s.attn_id == id && s.revision == rev => Some(s),
            (Some((attn_id, revision)), _) => Some(ShownCard {
                attn_id,
                revision,
                since: now,
            }),
            (None, _) => None,
        };
        // A row a tap armed stays armed only while the item it was armed on is the popup's and
        // on screen in full: once another item took its place, or it was cut short or covered,
        // even for a frame, it has to be picked again.
        let item = self
            .selected_attention()
            .map(|i| (i.attn_id.clone(), i.revision));
        let on_screen = match &item {
            None => true,
            Some((id, rev)) => self
                .view
                .shown
                .as_ref()
                .is_some_and(|s| s.attn_id == *id && s.revision == *rev),
        };
        if let Some(p) = self.view.action.as_mut() {
            if p.armed
                .as_ref()
                .is_some_and(|(_, armed)| *armed != item || !on_screen)
            {
                p.armed = None;
            }
        }
    }

    /// The `seen_gseq` of a message typed now ([`Action::Say`]): what the user has seen. The
    /// hub counts every item raised at or before it as seen (a relay of "allow both" may cover
    /// it), so it is [`AppState::last_gseq`] capped below the first raise of every open
    /// blocking item the last frame did not draw in full in its current revision
    /// ([`crate::select::DrawnItem`]: a card scrolled out of view, a thread's second item, an
    /// item raised or raised again since that frame, the orchestrator's banner outside the
    /// main conversation, anything under help, the picker, the `/` or `@` popup or the action
    /// popup). The cap is
    /// the item's first raise, not its latest: the hub counts an item as waiting since its
    /// first raise, and an earlier revision the TUI cannot show was drawn is unseen too. An
    /// item known only by its capped preview ([`Incomplete`]) is never drawn in full. Items
    /// raised after such an item count as unseen as well, drawn or not: the hub knows one
    /// position only.
    pub fn seen_gseq(&self) -> u64 {
        let drawn = |i: &AttentionItem| {
            self.incomplete(i).is_none()
                && self
                    .view
                    .panes
                    .items
                    .iter()
                    .any(|d| d.attn_id == i.attn_id && d.revision == i.revision)
        };
        self.attention
            .ordered(&self.view.pins)
            .into_iter()
            .filter(|i| i.reason.is_blocking() && !drawn(i))
            .map(|i| i.first_revision.min(i.revision).saturating_sub(1))
            .fold(self.last_gseq, u64::min)
    }

    /// The most urgent open attention item card `thread` shows ([`AppState::card_items`]: on
    /// the orchestrator's banner a confirm card comes first).
    pub fn top_attention(&self, thread: &str) -> Option<&AttentionItem> {
        self.card_items(thread).into_iter().next()
    }

    /// Keeps the selection on something the current view draws ([`AppState::nav_order`]): a
    /// card it no longer draws is not selected any more (none is selected in its place). The
    /// decisions overlay draws no card: the selection is kept for the screen it returns to.
    pub fn ensure_focus(&mut self) {
        if self.view.screen == Screen::Decisions {
            return;
        }
        let nav = self.nav_order();
        // A card whose detail view the main pane shows stays selected though the view does not
        // draw it (the sidebar hidden).
        let valid = self.view.focus.as_deref().is_none_or(|id| {
            nav.iter().any(|n| n == id) || self.view.detail.as_deref() == Some(id)
        });
        // The conversation selected (Shift-←): only the card it returns to goes.
        if !valid {
            if self.view.main_focus {
                self.view.focus = None;
            } else {
                self.set_focus(None);
            }
        }
    }

    /// Selects `thread` (a card), or nothing. The main pane does not change.
    pub fn set_focus(&mut self, thread: Option<String>) {
        self.view.request = None;
        self.view.block = None;
        self.view.queued = None;
        self.view.detail = None;
        // Another card selected: the cards follow the selection again.
        if self.view.focus != thread {
            self.view.card_scroll = None;
        }
        self.view.focus = thread;
    }

    /// Whether a dive hides the cards: in the stack view on a screen too narrow for their
    /// column (as last drawn), where they would be a row above it. The dive's own card is the
    /// selected one meanwhile ([`AppState::note_panes`]), and no key selects another.
    pub fn dive_hides_cards(&self) -> bool {
        // The compact layout's lists show the cards whatever the dive.
        if self.compact() && self.view.compact_pane != CompactPane::Chat {
            return false;
        }
        // Not before the first draw: no width yet.
        self.view.dive.is_some()
            && self.view.screen == Screen::Stack
            && !self.view.sidebar_hidden
            && (1..crate::ui::SIDE_MIN_WIDTH).contains(&self.view.width)
    }

    /// Whether the main pane is selected ([`ViewState::main_focus`]): only in the stack view,
    /// where it is drawn.
    pub fn main_focused(&self) -> bool {
        self.view.main_focus && self.view.screen == Screen::Stack
    }

    /// Shift-←: selects the main pane, keeping the selected card to return to.
    fn focus_main(&mut self) {
        self.view.main_focus = true;
    }

    /// Opens the help overlay at its top.
    fn open_help(&mut self) {
        self.view.help = true;
        self.view.help_scroll = 0;
    }

    /// Shift-→: the cards again, the one selected before Shift-← (or the first, when there was
    /// none or it is gone). What was selected in the conversation (a long block, a request) is
    /// not any more; the view stays where it is.
    fn focus_cards(&mut self) {
        if self.view.sidebar_hidden {
            self.view.status = Some("the thread sidebar is hidden (Ctrl-B shows it)".into());
            return;
        }
        if self.dive_hides_cards() {
            self.view.status = Some(DIVE_HIDES_CARDS.into());
            return;
        }
        self.view.main_focus = false;
        self.release_request();
        self.view.block = None;
        self.ensure_focus();
        if self.view.focus.is_none() {
            let first = self.nav_order().into_iter().next();
            self.set_focus(first);
        }
    }

    /// Selects the card `delta` places after the selected one in the current view's order,
    /// wrapping around; with none selected, the first (forward) or the last (backward).
    fn move_focus(&mut self, delta: isize) {
        let listed = self.compact() && self.view.compact_pane != CompactPane::Chat;
        if self.dive_hides_cards() && !listed {
            self.view.status = Some(DIVE_HIDES_CARDS.into());
            return;
        }
        let order = self.nav_order();
        if order.is_empty() {
            return;
        }
        let n = order.len() as isize;
        let next = match self
            .focused()
            .and_then(|f| order.iter().position(|id| id == f))
        {
            Some(i) => (i as isize + delta).rem_euclid(n) as usize,
            None if delta < 0 => order.len() - 1,
            None => 0,
        };
        self.set_focus(Some(order[next].clone()));
    }

    /// Moves the grid's selection one row down or up ([`grid_step_vertical`]); with none
    /// selected, the first tile (down) or the last (up).
    fn move_focus_vertical(&mut self, down: bool) {
        let order = self.grid_order();
        if order.is_empty() {
            return;
        }
        let next = match self
            .focused()
            .and_then(|f| order.iter().position(|id| id == f))
        {
            Some(cur) => grid_step_vertical(cur, order.len(), grid_columns(self.view.width), down),
            None if down => 0,
            None => order.len() - 1,
        };
        self.set_focus(Some(order[next].clone()));
    }

    /// The thread a command names (`query`: a name, alias or id, or a name it had before a
    /// rename, resolved as the hub resolves it, [`Registry::resolve`], but never by a prefix;
    /// the current orchestrator before a retired one), else the one dived into, else the
    /// selected card's. The error is for the status bar: no such thread, or several.
    pub fn thread_arg(&self, query: Option<&str>) -> Result<&ThreadView, String> {
        let Some(q) = query else {
            return self
                .view
                .dive
                .as_deref()
                .or(self.focused())
                .filter(|f| *f != HUB_THREAD)
                .and_then(|f| self.registry.get(f))
                .ok_or_else(|| "no thread selected".to_string());
        };
        let exact = |t: &&ThreadView| {
            t.id == q
                || t.name == q
                || t.aliases.iter().any(|a| a == q)
                || t.former_names.iter().any(|n| n == q)
        };
        match self.registry.resolve(q) {
            Ok(t) if exact(&t) => Ok(t),
            Err(ResolveError::Ambiguous { candidates, .. })
                if self.registry.list().iter().any(exact) =>
            {
                Err(format!(
                    "{q} names several threads: {}",
                    candidates.join(", ")
                ))
            }
            _ => Err(format!("no thread {q}")),
        }
    }

    /// Asks for `y` before parking (`park`) or resuming thread `query` (default: the one dived
    /// into, else the selected one).
    fn confirm_lifecycle_of(&mut self, park: bool, query: Option<&str>) {
        let t = match self.thread_arg(query) {
            Ok(t) => t,
            Err(e) => {
                self.view.status = Some(e);
                return;
            }
        };
        let (name, thread) = (t.name.clone(), t.id.clone());
        let (verb, action) = match (park, t.parked) {
            (true, true) => {
                self.view.status = Some(format!("{name} is already parked"));
                return;
            }
            (false, false) => {
                self.view.status = Some(format!("{name} is not parked"));
                return;
            }
            (true, false) => ("park", Action::Park { thread }),
            (false, true) => ("resume", Action::Resume { thread }),
        };
        self.view.status = None;
        self.view.confirm = Some(Confirm {
            action,
            prompt: format!("{verb} {name}? y to confirm, any other key cancels"),
            pending: format!("{verb} {name}: waiting for the hub"),
            alt: None,
        });
    }

    /// Asks for `y` before switching the orchestrator (`/orch`, or the picker `/orch` opens);
    /// `s` switches without the current orchestrator's summary.
    fn confirm_switch(&mut self, runtime: String, model: Option<String>) {
        let with = model
            .as_deref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();
        self.view.status = None;
        self.view.confirm = Some(Confirm {
            prompt: format!(
                "switch the orchestrator to {runtime}{with}? A new session that starts with a \
                 handoff. y: with the current one's summary (waits up to 60s if it is busy) · \
                 s: without it · any other key cancels"
            ),
            pending: format!(
                "switching the orchestrator to {runtime}: asking the current one for a handoff \
                 summary (up to 60s if it is busy)"
            ),
            alt: Some((
                's',
                Action::UseOrchestrator {
                    runtime: runtime.clone(),
                    model: model.clone(),
                    summary: false,
                },
                format!(
                    "switching the orchestrator to {runtime} (no summary): waiting for the hub"
                ),
            )),
            action: Action::UseOrchestrator {
                runtime,
                model,
                summary: true,
            },
        });
    }

    /// Runs a slash command typed on the input line (see [`crate::slash`]). Returns the
    /// actions to run now (a confirmation or a status line otherwise) and whether the line is
    /// done with (cleared).
    fn run_slash(&mut self, line: &str) -> (Vec<Action>, bool) {
        let cmd = match slash::parse(line) {
            Ok(c) => c,
            Err(e) => {
                self.view.status = Some(e);
                return (Vec::new(), false);
            }
        };
        let thread = |st: &mut AppState, q: &Option<String>| match st.thread_arg(q.as_deref()) {
            Ok(t) => Some((t.id.clone(), t.name.clone())),
            Err(e) => {
                st.view.status = Some(e);
                None
            }
        };
        match cmd {
            Slash::Help => {
                self.open_help();
                self.view.status = None;
            }
            Slash::Open(q) => {
                let target = match q {
                    Some(q) => self.thread_arg(Some(&q)).map(|t| t.id.clone()),
                    None => self
                        .focused()
                        .filter(|f| *f != HUB_THREAD)
                        .map(str::to_string)
                        .ok_or_else(|| "no card selected; /open <thread>".to_string()),
                };
                match target {
                    Ok(id) => self.dive(&id),
                    Err(e) => {
                        self.view.status = Some(e);
                        return (Vec::new(), false);
                    }
                }
            }
            Slash::Back => {
                self.leave_dive();
                self.view.status = None;
            }
            Slash::Unsend => return (self.unsend(), true),
            Slash::Mode { thread, mode } => {
                let target = match (thread, &self.view.dive) {
                    (Some(q), _) => self.thread_arg(Some(&q)).map(|t| t.id.clone()),
                    (None, Some(d)) => Ok(d.clone()),
                    (None, None) => {
                        Err("usage: /mode <thread> <mode> (in a dive: /mode <mode>)".to_string())
                    }
                };
                let thread = match target {
                    Ok(t) => t,
                    Err(e) => {
                        self.view.status = Some(e);
                        return (Vec::new(), false);
                    }
                };
                let name = self
                    .registry
                    .get(&thread)
                    .map_or(thread.clone(), |t| t.name.clone());
                self.view.status = Some(format!("mode of {name} → {mode}: waiting for the hub"));
                return (vec![Action::SetMode { thread, mode }], true);
            }
            Slash::Model {
                thread,
                model,
                effort,
                runtime,
            } => {
                let target = match (thread, &self.view.dive) {
                    (Some(q), _) => self.thread_arg(Some(&q)).map(|t| t.id.clone()),
                    (None, Some(d)) => Ok(d.clone()),
                    (None, None) => Err(
                        "usage: /model <thread> [model] [--effort e] [--runtime r] (in a dive: \
                         /model [--effort e] [--runtime r])"
                            .to_string(),
                    ),
                };
                let thread = match target {
                    Ok(t) => t,
                    Err(e) => {
                        self.view.status = Some(e);
                        return (Vec::new(), false);
                    }
                };
                let view = self.registry.get(&thread);
                if model.is_none() && effort.is_none() && runtime.is_none() {
                    self.view.status = Some(match view {
                        Some(t) => model_show(t),
                        None => format!("no thread {thread}"),
                    });
                    return (Vec::new(), true);
                }
                let name = view.map_or(thread.clone(), |t| t.name.clone());
                let what: Vec<String> = [
                    ("runtime", &runtime),
                    ("model", &model),
                    ("effort", &effort),
                ]
                .into_iter()
                .filter_map(|(k, v)| v.as_deref().map(|v| format!("{k} {v}")))
                .collect();
                self.view.status =
                    Some(format!("{name} → {}: waiting for the hub", what.join(", ")));
                return (
                    vec![Action::SetModel {
                        thread,
                        model,
                        effort,
                        runtime,
                    }],
                    true,
                );
            }
            Slash::Rename { thread, name } => {
                let target = match (thread, &self.view.dive) {
                    (Some(q), _) => self.thread_arg(Some(&q)).map(|t| t.id.clone()),
                    (None, Some(d)) => Ok(d.clone()),
                    (None, None) => {
                        Err("usage: /rename <thread> <name> (in a dive: /rename <name>)"
                            .to_string())
                    }
                };
                let thread = match target {
                    Ok(t) => t,
                    Err(e) => {
                        self.view.status = Some(e);
                        return (Vec::new(), false);
                    }
                };
                let old = self
                    .registry
                    .get(&thread)
                    .map_or(thread.clone(), |t| t.name.clone());
                self.view.status = Some(format!("renaming {old} → {name}: waiting for the hub"));
                return (vec![Action::Rename { thread, name }], true);
            }
            Slash::Clear => {
                self.view.confirm = Some(Confirm {
                    action: Action::ResetOrchestrator,
                    prompt: "Clear the orchestrator conversation? History stays in the log; workers stay. [y/N]".into(),
                    pending: "clearing the orchestrator conversation...".into(),
                    alt: None,
                });
            }
            Slash::Orch { runtime, model } => self.confirm_switch(runtime, model),
            Slash::OrchPick => self.open_orch_picker(),
            Slash::Grid => self.toggle_grid(),
            Slash::Decisions => self.toggle_decisions(),
            Slash::Pin(q) => {
                if let Err(e) = self.toggle_pin(q.as_deref()) {
                    self.view.status = Some(e);
                    return (Vec::new(), false);
                }
            }
            Slash::Quit => return (vec![Action::Quit], true),
            Slash::New {
                project,
                runtime,
                name,
            } => {
                self.view.status = Some(format!("creating a thread in {project}..."));
                return (
                    vec![Action::NewThread {
                        project,
                        runtime,
                        name,
                    }],
                    true,
                );
            }
            Slash::Park(q) => {
                self.confirm_lifecycle_of(true, q.as_deref());
                if self.view.confirm.is_none() {
                    return (Vec::new(), false);
                }
            }
            Slash::Resume(q) => {
                let t = match self.thread_arg(q.as_deref()) {
                    Ok(t) => t,
                    Err(e) => {
                        self.view.status = Some(e);
                        return (Vec::new(), false);
                    }
                };
                if !t.parked {
                    self.view.status = Some(format!("{} is not parked", t.name));
                    return (Vec::new(), false);
                }
                let (thread, name) = (t.id.clone(), t.name.clone());
                self.view.status = Some(format!("resume {name}: waiting for the hub"));
                return (vec![Action::Resume { thread }], true);
            }
            Slash::Trim(q) => {
                let t = match self.thread_arg(q.as_deref()) {
                    Ok(t) => t,
                    Err(e) => {
                        self.view.status = Some(e);
                        return (Vec::new(), false);
                    }
                };
                let (thread, name) = (t.id.clone(), t.name.clone());
                self.view.status = Some(format!("trim {name}: deleting its build output"));
                return (vec![Action::Trim { thread }], true);
            }
            Slash::Stop(q) => {
                let Some((thread, name)) = thread(self, &q) else {
                    return (Vec::new(), false);
                };
                self.view.status = None;
                self.view.confirm = Some(Confirm {
                    action: Action::Stop { thread },
                    prompt: format!("stop {name}? y to confirm, any other key cancels"),
                    pending: format!("stop {name}: waiting for the hub"),
                    alt: None,
                });
            }
            Slash::Delete(qs) => {
                let (mut threads, mut names) = (Vec::new(), Vec::new());
                for q in qs {
                    let Some((id, name)) = thread(self, &Some(q)) else {
                        return (Vec::new(), false);
                    };
                    if !threads.contains(&id) {
                        threads.push(id);
                        names.push(name);
                    }
                }
                // The delete card shows with the main conversation, never in a dive.
                self.back();
                self.view.status =
                    Some(format!("delete {}: waiting for the hub", names.join(", ")));
                return (vec![Action::Delete { threads }], true);
            }
            Slash::Trash(TrashCmd::List) => {
                self.view.status = Some("trash: waiting for the hub".into());
                return (vec![Action::TrashList], true);
            }
            // The question comes with the hub's answer: it lists the items `y` purges, by id.
            Slash::Trash(TrashCmd::Empty { items, all }) => {
                self.view.status = Some("trash: looking up what to purge".into());
                return (vec![Action::TrashChoose { items, all }], true);
            }
            Slash::Trash(TrashCmd::Restore { item, as_name }) => {
                self.view.status = Some(format!("restoring {item}: waiting for the hub"));
                return (vec![Action::TrashRestore { item, as_name }], true);
            }
            Slash::Parked(on) => self.show_parked(on.unwrap_or(!self.view.show_parked)),
            Slash::Sidebar(on) => self.set_sidebar(on.unwrap_or(self.view.sidebar_hidden)),
            Slash::Compact(on) => {
                let on = on.unwrap_or(!self.compact());
                self.view.compact = Some(on);
                self.fit_compact();
                self.view.status = Some(if on {
                    "compact layout: /c the conversation, /n what needs you, /t the threads".into()
                } else {
                    "full layout (/compact on: one screen at a time)".into()
                });
                self.ensure_focus();
            }
            Slash::Pane(pane) => {
                if self.compact() {
                    self.show_pane(pane);
                } else {
                    self.view.status = Some(
                        "/c, /n and /t pick the compact layout's screen: /compact turns it on"
                            .into(),
                    );
                }
            }
            Slash::Focus(on) => self.set_focus_mode(on.unwrap_or(!self.view.focus_mode)),
            Slash::Suggest(on) => {
                self.toggle_suggest(on);
                return (Vec::new(), true);
            }
            Slash::Mouse(on) => {
                let on = on.unwrap_or(!self.view.mouse);
                self.view.mouse = on;
                self.view.selection = None;
                self.view.status = Some(if on {
                    "mouse on: drag to select and copy, wheel to scroll (Shift-drag: the \
                     terminal's own selection)"
                        .into()
                } else {
                    "mouse off: the terminal selects and scrolls (/mouse turns it back on)".into()
                });
                return (vec![Action::Mouse(on)], true);
            }
            Slash::Attach(q) => {
                let Some((_, name)) = thread(self, &q) else {
                    return (Vec::new(), false);
                };
                // The TUI cannot hand its terminal over: the attach runs in another one.
                self.view.status = Some(format!(
                    "to attach, run in another terminal: twr{} attach {name}",
                    self.attach_via
                ));
            }
            Slash::Ws(None) => self.view.status = Some(self.workspaces_line()),
            Slash::Ws(Some(name)) => return (self.switch_workspace(&name), true),
        }
        (Vec::new(), true)
    }

    /// Runtimes that can host the orchestrator (for `/orch` completion and its picker).
    pub fn runtimes(&self) -> Vec<String> {
        if self.view.orch_runtimes.is_empty() {
            ORCH_RUNTIMES.iter().map(|s| s.to_string()).collect()
        } else {
            self.view.orch_runtimes.clone()
        }
    }

    /// `/orch` without arguments: the picker of the orchestrator's runtime
    /// ([`Picker::switch`]), the current one highlighted (before an orchestrator thread
    /// exists, the one the hub would start); choosing asks to confirm the switch.
    fn open_orch_picker(&mut self) {
        let current = self
            .orchestrator()
            .map(|o| o.runtime.clone())
            .or_else(|| self.view.orch_default.as_ref().map(|(r, _)| r.clone()));
        let runtimes: Vec<(String, Option<bool>)> = self
            .runtimes()
            .into_iter()
            .map(|r| {
                let installed = self
                    .view
                    .orch_installed
                    .iter()
                    .find(|(n, _)| *n == r)
                    .map(|(_, i)| *i);
                (r, installed)
            })
            .collect();
        let default = current.unwrap_or_default();
        let selected = runtimes
            .iter()
            .position(|(r, _)| *r == default)
            .unwrap_or(0);
        self.view.status = None;
        self.view.picker = Some(Picker {
            runtimes,
            selected,
            default,
            model: None,
            switch: true,
        });
    }

    /// `/pin [thread]`: pins or unpins the thread named (`query`), else the selected card.
    /// Pins never leave the client; an unpinned parked thread may leave the listing
    /// ([`AppState::hides`]). The error is for the status bar.
    fn toggle_pin(&mut self, query: Option<&str>) -> Result<(), String> {
        let (id, name) = match query {
            Some(q) => {
                let t = self.thread_arg(Some(q))?;
                if t.is_orchestrator() {
                    return Err("the orchestrator has no card to pin".into());
                }
                (t.id.clone(), t.name.clone())
            }
            None if self.main_focused() => return Err(MAIN_SELECTED.into()),
            None => {
                let id = self
                    .focused()
                    .filter(|f| self.registry.get(f).is_some_and(|t| !t.is_orchestrator()))
                    .ok_or_else(|| "no card selected; /pin <thread>".to_string())?
                    .to_string();
                let name = self
                    .registry
                    .get(&id)
                    .map_or(id.clone(), |t| t.name.clone());
                (id, name)
            }
        };
        let pinned = !self.view.pins.remove(&id);
        if pinned {
            self.view.pins.insert(id);
        }
        // Your own move: the cards take their order now.
        self.view.shown_cards = None;
        self.view.status = Some(format!(
            "{} {name}",
            if pinned { "pinned" } else { "unpinned" }
        ));
        self.ensure_focus();
        Ok(())
    }

    /// Tab on a `/` line: completes a command, a runtime or a thread name.
    fn complete_slash(&mut self) {
        let threads: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|t| !t.retired)
            .map(|t| t.name.clone())
            .collect();
        let workspaces: Vec<String> = self.tabs.iter().map(|t| t.name.clone()).collect();
        let (line, candidates) =
            slash::complete(&self.view.input, &self.runtimes(), &threads, &workspaces);
        if let Some(l) = line {
            self.view.input.set(&l);
            self.view
                .input
                .keep_visible(self.input_cols(), self.input_rows());
        }
        self.view.status = (!candidates.is_empty()).then(|| candidates.join("  "));
    }

    /// Handles one key press and returns what the IO loop must do.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        let view = self.armed_view();
        let armed = self.sent.as_ref().and_then(|s| s.armed);
        let actions = self.key(key);
        self.sync_action();
        self.disarm_on_view_change(&view, armed);
        actions
    }

    /// A double Esc armed before an input that changed the view (`view` before it, the arm
    /// `armed` then) is for no view any more, even once that one shows again (`Ctrl-G` twice);
    /// unless the input armed it anew, as the Esc that leaves a dive does.
    fn disarm_on_view_change(&mut self, view: &ArmedView, armed: Option<Instant>) {
        if self.armed_view() == *view {
            return;
        }
        if let Some(s) = self.sent.as_mut().filter(|s| s.armed == armed) {
            s.armed = None;
        }
    }

    /// [`AppState::handle_key`], before the action popup is checked against the selection.
    fn key(&mut self, key: KeyEvent) -> Vec<Action> {
        // Any key ends a mouse selection (its text was copied when the button went up).
        self.view.selection = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-C stops a `!` command this TUI runs on the main pane's thread, else quits.
        if ctrl && key.code == KeyCode::Char('c') {
            return self.ctrl_c();
        }
        // Ctrl-L repaints the whole screen, whatever else is open.
        if ctrl && key.code == KeyCode::Char('l') {
            self.view.repaint = true;
            return Vec::new();
        }
        // Workspaces: Alt-1..9 the tab at that place, Ctrl-PgUp/PgDn the previous and next one
        // (only while the strip shows), whatever is open: the first-run picker of a new
        // workspace must not keep the user in it.
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c @ '1'..='9') if alt && !ctrl && self.tabs_shown() => {
                return self.switch_tab(usize::from(c as u8 - b'1'));
            }
            KeyCode::PageUp | KeyCode::PageDown if ctrl && self.tabs_shown() => {
                return self.step_tab(key.code == KeyCode::PageDown);
            }
            _ => {}
        }
        if self.picker_open() {
            return self.picker_key(key);
        }
        // The help is drawn over the trash: it takes the key first. Both scroll with the
        // arrows, PgUp/PgDn and Home/End; any other key closes them.
        if self.view.help {
            if !overlay_scroll(
                &mut self.view.help_scroll,
                self.view.panes.help_max,
                key.code,
            ) {
                self.view.help = false;
            }
            return Vec::new();
        }
        if self.view.trash.is_some() {
            if !overlay_scroll(
                &mut self.view.trash_scroll,
                self.view.panes.trash_max,
                key.code,
            ) {
                self.view.trash = None;
            }
            return Vec::new();
        }
        // A pending confirmation eats the next key: `y` runs it (or its alternative's key
        // its alternative), anything else cancels.
        if let Some(c) = self.view.confirm.take() {
            if !ctrl && matches!(key.code, KeyCode::Char('y' | 'Y')) {
                self.view.status = Some(c.pending);
                return vec![c.action];
            }
            if let (Some((k, action, pending)), KeyCode::Char(got)) = (c.alt, key.code) {
                if !ctrl && got.eq_ignore_ascii_case(&k) {
                    self.view.status = Some(pending);
                    return vec![action];
                }
            }
            self.view.status = Some("cancelled".into());
            return Vec::new();
        }
        // The decisions overlay has keys of its own, and no input box to type into.
        if self.view.screen == Screen::Decisions {
            if let Some(actions) = self.decisions_key(key) {
                return actions;
            }
            return self.view_key(key).unwrap_or_default();
        }
        // The action popup takes its keys first; it types nothing.
        if self.action_popup().is_some() {
            if let Some(actions) = self.action_key(key) {
                return actions;
            }
            if let Some(actions) = self.selection_key(key) {
                self.follow_action();
                return actions;
            }
            return self.view_key(key).unwrap_or_default();
        }
        // A line break goes in before the popup could take the Enter.
        if self.newline(key) {
            return Vec::new();
        }
        // Tab or → on the empty line takes the suggestion shown in it ([`crate::suggest`]).
        if matches!(key.code, KeyCode::Tab | KeyCode::Right)
            && key.modifiers.is_empty()
            && self.accept_suggestion()
        {
            return Vec::new();
        }
        // The slash command popup takes its keys first.
        if let Some(actions) = self.popup_key(key) {
            return actions;
        }
        if let Some(actions) = self.selection_key(key) {
            return actions;
        }
        // The compact layout's lists: up and down on an empty line move through them.
        if self.compact_listed()
            && self.view.input.is_empty()
            && !ctrl
            && !alt
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            self.view.main_focus = false;
            self.move_focus(if key.code == KeyCode::Up { -1 } else { 1 });
            return Vec::new();
        }
        if let Some(actions) = self.view_key(key) {
            return actions;
        }
        let empty = self.view.input.is_empty() && !ctrl && !alt;
        match key.code {
            // Esc closes, releases or leaves something: never the text typed.
            KeyCode::Esc => return self.esc(),
            // On an empty line, Enter is about the selection: a long block expands or
            // collapses, anything else opens its action popup. Nothing selected: nothing.
            // In the compact layout's conversation, and with nothing selected on its lists,
            // Enter on an empty line opens the oldest item waiting on the user.
            KeyCode::Enter
                if empty
                    && self.compact()
                    && self.view.block.is_none()
                    && self.view.request.is_none()
                    && self.view.queued.is_none()
                    && self.detail_card().is_none()
                    && (self.view.compact_pane == CompactPane::Chat
                        || self.selection_target().is_none()) =>
            {
                self.open_oldest_waiting();
                return Vec::new();
            }
            KeyCode::Enter if empty => {
                if self.view.screen == Screen::Stack && self.view.block.is_some() {
                    self.toggle_block(self.view.block.clone());
                } else if let Some(target) = self.selection_target() {
                    self.open_action(target);
                }
                return Vec::new();
            }
            // Typed on an empty line, `?` opens the help; after text it is a character.
            KeyCode::Char('?') if empty => {
                self.open_help();
                return Vec::new();
            }
            // End jumps to the bottom of the main pane (and follows it), Home to its top: on
            // an empty line (where they would move no cursor), or with Ctrl.
            KeyCode::End | KeyCode::Home
                if self.view.panes.card.is_none() && (ctrl || self.view.input.is_empty()) =>
            {
                if key.code == KeyCode::End {
                    self.view.request = None;
                    self.view.shown = None;
                    self.follow();
                } else {
                    self.scroll_top();
                }
                return Vec::new();
            }
            _ => {}
        }
        self.input_key(key)
    }

    /// The keys about the selection, which type nothing: Tab (unless there is something to
    /// complete: the `/` line, the `/` or `@` popup takes it first), Shift-Tab and the
    /// Shift-arrows. In the stack view Tab and Shift-↑/↓ move between the cards, Shift-←
    /// selects the main pane and Shift-→ the cards again; with the main pane selected,
    /// Shift-↑/↓ step between its long blocks and requests ([`AppState::step_main`]). In the
    /// grid the Shift-arrows move between tiles in two dimensions. `None`: not one of them.
    fn selection_key(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let others = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let grid = self.view.screen == Screen::Grid;
        let compact = self.compact() && self.view.screen != Screen::Decisions;
        let listed = compact && self.view.compact_pane != CompactPane::Chat;
        let chat = compact && self.view.compact_pane == CompactPane::Chat;
        match key.code {
            // On a slash command line, Tab completes it.
            KeyCode::Tab
                if !others && self.action_popup().is_none() && self.view.input.starts_with('/') =>
            {
                self.complete_slash();
            }
            KeyCode::Tab | KeyCode::BackTab if !others && chat => {
                self.view.status = Some(COMPACT_LISTS.into());
            }
            KeyCode::Tab | KeyCode::BackTab if !others => {
                self.view.main_focus = false;
                self.move_focus(if key.code == KeyCode::Tab { 1 } else { -1 });
            }
            // The compact layout's lists: Shift-↑/↓ move through the rows, as ↑/↓ do; there is
            // no main pane beside them.
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
                if shift && !others && listed =>
            {
                if matches!(key.code, KeyCode::Up | KeyCode::Down) {
                    self.view.main_focus = false;
                    self.move_focus(if key.code == KeyCode::Up { -1 } else { 1 });
                }
            }
            // Its conversation draws no cards: Shift-↑/↓ step between the long blocks and
            // requests, Shift-→ points at the lists.
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
                if shift && !others && chat =>
            {
                match key.code {
                    KeyCode::Right => {
                        self.view.status = Some(COMPACT_LISTS.into());
                    }
                    KeyCode::Left => self.focus_main(),
                    _ => {
                        self.view.main_focus = true;
                        self.step_main(key.code == KeyCode::Up);
                    }
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right if shift && !others => {
                let main = self.main_focused();
                match key.code {
                    KeyCode::Right if grid => self.move_focus(1),
                    KeyCode::Left if grid => self.move_focus(-1),
                    KeyCode::Down if grid => self.move_focus_vertical(true),
                    KeyCode::Up if grid => self.move_focus_vertical(false),
                    KeyCode::Right => self.focus_cards(),
                    KeyCode::Left => self.focus_main(),
                    KeyCode::Up | KeyCode::Down if main => {
                        self.step_main(key.code == KeyCode::Up);
                    }
                    KeyCode::Up => self.move_focus(-1),
                    _ => self.move_focus(1),
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// Keys about the view, whatever is selected or typed: Ctrl-B (the sidebar), Ctrl-F
    /// (Focus mode), Ctrl-O (tool calls), Ctrl-G (stack or grid), PgUp/PgDn (a page of the
    /// main pane, or of a detail view that takes it), and Ctrl-↑/↓ or Alt-↑/↓ (a line of it;
    /// Alt for macOS, whose Ctrl-↑/↓ belong to Mission Control). `None`: not one of them.
    fn view_key(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('b') if ctrl => self.set_sidebar(self.view.sidebar_hidden),
            KeyCode::Char('f') if ctrl => self.set_focus_mode(!self.view.focus_mode),
            KeyCode::Char('o') if ctrl => {
                self.view.expand_all = !self.view.expand_all;
                // One setting for the orchestrator conversation and every dive.
                self.view.status = Some(
                    if self.view.expand_all {
                        "showing tool calls and the hub's updates, here and in every thread \
                         (Ctrl-O folds them)"
                    } else {
                        "folding tool calls and the hub's updates, here and in every thread \
                         (Ctrl-O shows them)"
                    }
                    .into(),
                );
            }
            KeyCode::Char('g') if ctrl && self.view.screen != Screen::Decisions => {
                self.toggle_grid();
            }
            // A detail view in the main pane scrolls instead of the transcript under it.
            KeyCode::PageUp | KeyCode::PageDown if self.view.panes.card.is_some() => {
                self.scroll_card(key.code == KeyCode::PageDown, usize::MAX);
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                self.scroll_transcript(key.code == KeyCode::PageDown, SCROLL_STEP);
            }
            KeyCode::Up | KeyCode::Down if ctrl || alt => {
                let down = key.code == KeyCode::Down;
                if self.view.panes.card.is_some() {
                    self.scroll_card(down, 1);
                } else {
                    self.scroll_line(down);
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// Esc does the first of these that applies: closes the open popup (the `/` popup closes
    /// itself, [`AppState::popup_key`]); takes back the last message sent (the double Esc,
    /// [`AppState::unsend_esc`]); releases the selection, a long block, a request, the main
    /// pane, then the selected card; in a dive, leaves it. It never clears the text typed.
    fn esc(&mut self) -> Vec<Action> {
        // In step with what the screen says it does ([`AppState::esc_does`]).
        if self.view.action.take().is_some() {
            return Vec::new();
        }
        if let Some(actions) = self.unsend_esc(Instant::now()) {
            return actions;
        }
        if self.view.queued.take().is_some() {
            return Vec::new();
        }
        if self.view.screen == Screen::Stack && self.view.block.take().is_some() {
            return Vec::new();
        }
        if self.view.request.take().is_some() {
            self.view.shown = None;
            self.view.detail = None;
            self.follow();
            return Vec::new();
        }
        // With the main pane selected in a dive, Esc leaves it: Shift-← then Esc does.
        if self.main_focused() && self.view.dive.is_none() {
            self.view.main_focus = false;
            self.set_focus(None);
            return Vec::new();
        }
        if self.view.focus.is_some() && !self.main_focused() && !self.forced_focus() {
            self.set_focus(None);
            return Vec::new();
        }
        if self.view.dive.is_some() {
            self.leave_dive();
            self.view.status = None;
        }
        Vec::new()
    }

    /// What the next Esc does, as [`AppState::esc`] would do it: for the words that say so
    /// (the dive's header, the input box's title, the hints).
    pub fn esc_does(&self) -> EscDoes {
        let v = &self.view;
        if self.picker_open()
            || v.help
            || v.trash.is_some()
            || v.confirm.is_some()
            || v.screen == Screen::Decisions
            || self.action_popup().is_some()
            || self.popup().is_some()
        {
            return EscDoes::Close;
        }
        let main = self.main_focused();
        if (v.screen == Screen::Stack && v.block.is_some())
            || v.queued.is_some()
            || v.request.is_some()
            || (main && v.dive.is_none())
            || (v.focus.is_some() && !main && !self.forced_focus())
        {
            return EscDoes::Release;
        }
        if v.dive.is_some() {
            EscDoes::Leave
        } else {
            EscDoes::Nothing
        }
    }

    /// The double Esc that takes back the last message sent, at `now`, with nothing typed:
    /// while the message can be taken back and is the main pane's thread's, the first Esc
    /// only arms the second (the input box's title shows the hint for [`UNSEND_WINDOW`]), and
    /// the second, within that time, takes it back ([`AppState::unsend`]). A hint that ran
    /// out is spent: for that message Esc goes on to its other steps. `None`: this Esc is not
    /// the double Esc's.
    ///
    /// The first Esc that leaves the dive the message went to leaves it at once: going back
    /// never waits for the second. It arms the second there, in the orchestrator conversation,
    /// and that one takes the message back and returns to its dive, where the text goes back
    /// in the box.
    fn unsend_esc(&mut self, now: Instant) -> Option<Vec<Action>> {
        if !self.view.input.is_empty() {
            return None;
        }
        let here = self.main_thread();
        let view = self.armed_view();
        if let Some(at) = self.sent.as_ref().and_then(|s| s.armed) {
            // Armed for another view than the one shown now (Ctrl-G, another screen): no
            // second Esc for it; this one does what Esc does here.
            if self
                .sent
                .as_ref()
                .is_some_and(|s| s.armed_in.as_ref() != Some(&view))
            {
                if let Some(s) = self.sent.as_mut() {
                    s.armed = None;
                }
                return None;
            }
            if now.saturating_duration_since(at) < UNSEND_WINDOW {
                return Some(self.unsend_armed());
            }
            self.expire_unsend(now);
            return None;
        }
        let s = self.sent.as_ref()?;
        if s.spent || s.thread != here || self.unsend_check().is_err() {
            return None;
        }
        let armed_in = if self.esc_does() == EscDoes::Leave {
            self.leave_dive();
            self.view.status = None;
            self.armed_view()
        } else {
            view
        };
        let s = self.sent.as_mut()?;
        s.armed = Some(now);
        s.armed_in = Some(armed_in);
        Some(Vec::new())
    }

    /// The view an armed double Esc counts in, as it is now ([`Sent::armed_in`]).
    fn armed_view(&self) -> ArmedView {
        ArmedView {
            main: self.main_thread(),
            screen: self.view.screen,
            pane: self.view.compact_pane,
        }
    }

    /// The armed second Esc (or tap): takes the last message back ([`AppState::unsend`]).
    /// Armed by the Esc that left its dive, it returns there first, so the text goes back in
    /// the box it was typed in.
    fn unsend_armed(&mut self) -> Vec<Action> {
        let back_to = self
            .sent
            .as_ref()
            .filter(|s| s.armed_in.as_ref().is_some_and(|v| v.main != s.thread))
            .and_then(|s| s.thread.clone());
        let actions = self.unsend();
        if let Some(thread) = back_to.filter(|_| !actions.is_empty()) {
            self.dive(&thread);
        }
        actions
    }

    /// The input box's title while the double Esc is armed, nothing is typed, the main pane
    /// shows the message's thread and the message can still be taken back: what the second
    /// Esc does.
    pub fn unsend_hint(&self) -> Option<String> {
        let s = self.sent.as_ref().filter(|s| s.armed.is_some())?;
        if !self.view.input.is_empty()
            || s.armed_in.as_ref() != Some(&self.armed_view())
            || self.unsend_check().is_err()
        {
            return None;
        }
        let thread = s.thread.as_deref().and_then(|t| self.registry.get(t))?;
        Some(if thread.is_orchestrator() {
            " Esc or tap again: take back your last message ".to_string()
        } else if s.armed_in.as_ref().is_some_and(|v| v.main != s.thread) {
            // Back from its dive.
            format!(
                " Esc or tap again: take back your message to {} ",
                thread.name
            )
        } else {
            format!(
                " Esc or tap again: take back your message to {} · /back leaves ",
                thread.name
            )
        })
    }

    /// When the armed double Esc runs out, while it is armed.
    pub fn unsend_deadline(&self) -> Option<Instant> {
        self.sent
            .as_ref()
            .and_then(|s| s.armed)
            .map(|at| at + UNSEND_WINDOW)
    }

    /// Disarms the double Esc if its time ran out at `now`: its hint is spent.
    pub fn expire_unsend(&mut self, now: Instant) {
        if let Some(s) = self.sent.as_mut() {
            if s.armed
                .is_some_and(|at| now.saturating_duration_since(at) >= UNSEND_WINDOW)
            {
                s.armed = None;
                s.spent = true;
            }
        }
    }

    /// Whether the last message can still be taken back, and if so its thread and ref.
    /// What this client saw of its turn decides (started, acted, ended; a runtime that cannot
    /// interrupt); the hub decides for good, and refuses what it is too late for.
    pub fn unsend_check(&self) -> Result<(String, i64), NoUnsend> {
        let Some(s) = self.sent.as_ref().filter(|s| !s.withdrawing) else {
            return Err(NoUnsend::Nothing);
        };
        if let Some(why) = s.refused {
            return Err(why);
        }
        if s.applied {
            return Err(NoUnsend::Applied);
        }
        let (Some(thread), Some(msg_ref)) = (s.thread.as_ref(), s.msg_ref) else {
            return Err(NoUnsend::Nothing);
        };
        if s.acted || s.ended {
            return Err(NoUnsend::TooLate);
        }
        if let Some(turn) = s.turn {
            let Some(v) = self.registry.get(thread) else {
                return Err(NoUnsend::Nothing);
            };
            if v.current_turn != Some(turn) || v.current_turn_acted {
                return Err(NoUnsend::TooLate);
            }
            // Only a local headless runtime interrupts a turn.
            if v.box_name != "local" || v.runtime.starts_with("pty") {
                return Err(NoUnsend::Uninterruptible);
            }
        }
        Ok((thread.clone(), msg_ref))
    }

    /// Takes back the last message sent (`/unsend`, the second Esc): asks the hub
    /// (`message.withdraw`) while it still can be, else says why not in the status bar. The
    /// withdrawal is followed by its ref ([`AppState::withdrawals`]), whatever is sent next.
    pub fn unsend(&mut self) -> Vec<Action> {
        match self.unsend_check() {
            Ok((thread, msg_ref)) => {
                let Some(s) = self.sent.as_mut() else {
                    return Vec::new();
                };
                s.withdrawing = true;
                s.armed = None;
                self.withdrawals.push(Withdrawal {
                    thread: thread.clone(),
                    msg_ref,
                    text: s.text.clone(),
                    purpose: Retract::Unsend,
                    mine: true,
                    suggested: s.suggested,
                });
                vec![Action::Withdraw { thread, msg_ref }]
            }
            Err(why) => {
                let thread = self.sent.as_mut().and_then(|s| {
                    s.armed = None;
                    s.thread.clone()
                });
                let msg = self.no_unsend(why, thread.as_deref());
                self.set_timed_status(msg, Instant::now());
                Vec::new()
            }
        }
    }

    /// The status bar's words for `why` a message to `thread` cannot be taken back.
    fn no_unsend(&self, why: NoUnsend, thread: Option<&str>) -> String {
        let thread = thread.and_then(|t| self.registry.get(t));
        let name = thread.map_or("the thread", |t| t.name.as_str());
        match why {
            NoUnsend::Nothing => "nothing to take back".into(),
            NoUnsend::Applied => "too late to take back: the hub applied it at once".into(),
            NoUnsend::TooLate if thread.is_none_or(|t| t.is_orchestrator()) => {
                "too late to take back: the orchestrator already acted on it".into()
            }
            NoUnsend::TooLate => {
                format!(
                    "too late to take back: {name} already started on it · /stop {name} stops it"
                )
            }
            NoUnsend::Uninterruptible => {
                format!("can't take back: {name}'s runtime can't be interrupted")
            }
        }
    }

    /// A message the input line sends now, to the thread dived into, else to the
    /// orchestrator (which `say`'s result names): it is the last message from now on. `typed`
    /// is the line as typed (a `//` escape included), what taking it back puts back.
    fn note_sent(&mut self, thread: Option<String>, typed: &str, suggested: bool) {
        // A suggestion asked before this send is about a conversation that went on.
        self.suggest_epoch += 1;
        self.suggestion = None;
        self.sends_suggested.push_back(suggested);
        let waiting = self.sent.as_ref().map_or(0, |s| s.waiting) + 1;
        self.sent = Some(Sent {
            thread,
            text: typed.to_string(),
            waiting,
            suggested,
            ..Sent::default()
        });
    }

    /// What the hub answered to a message sent (`say`, `thread.send`; `failed` when the call
    /// failed): the last one's answer gives it its ref, its thread and, if it started one at
    /// once, its turn (or the turn its ref started already), and whether that turn acted
    /// already; a short command has none (applied at once).
    fn sent_result(&mut self, result: &Value) {
        // Queued or started, the thread's queue moved.
        if let Some(t) = result["thread"].as_str() {
            let t = t.to_string();
            self.queue_changed(&t);
        }
        // A suggestion's ref, whichever send this answers (a later send replaced `sent`).
        if self.sends_suggested.pop_front() == Some(true) {
            if let (Some(t), Some(r)) = (result["thread"].as_str(), result["ref"].as_i64()) {
                self.suggested_refs.push_back((t.to_string(), r));
            }
        }
        let Some(s) = self.sent.as_mut() else {
            return;
        };
        s.waiting = s.waiting.saturating_sub(1);
        if s.waiting > 0 {
            return;
        }
        s.applied = result["action"] == "command";
        s.msg_ref = result["ref"].as_i64();
        if let Some(t) = result["thread"].as_str() {
            s.thread = Some(t.to_string());
        }
        s.turn = result["turn"].as_u64().or_else(|| {
            let (thread, r) = (s.thread.as_deref()?, s.msg_ref?);
            self.started_refs
                .iter()
                .rev()
                .find(|(t, m, _)| t == thread && *m == r)
                .map(|(_, _, turn)| *turn)
        });
        if let (Some(thread), Some(turn)) = (s.thread.as_deref(), s.turn) {
            s.acted |= self
                .acted_turns
                .iter()
                .any(|(t, n)| t == thread && *n == turn);
        }
    }

    /// Follows the last message's turn through `ev` (either stream): the turn its ref started,
    /// whether it called a tool or raised a request, whether it ended. The turns started from
    /// a ref, and those that acted, are kept a while for a message whose answer comes later.
    fn track_sent(&mut self, ev: &Event) {
        if let EventBody::TurnStarted(p) = &ev.body {
            if let (Some(r), Some(turn)) = (message_ref(p), ev.turn) {
                // Running, it is out of the queue: no edit takes it back any more.
                self.suggested_refs
                    .retain(|(t, m)| !(*t == ev.thread && *m == r));
                let started = (ev.thread.clone(), r, turn);
                if !self.started_refs.contains(&started) {
                    self.started_refs.push_back(started);
                    if self.started_refs.len() > STARTED_REFS {
                        self.started_refs.pop_front();
                    }
                }
            }
        }
        let acts = match &ev.body {
            EventBody::AgentToolCall(_) => true,
            EventBody::AttentionRaised(p) => p.reason.is_blocking(),
            _ => false,
        };
        if let (true, Some(turn)) = (acts, ev.turn) {
            let acted = (ev.thread.clone(), turn);
            if !self.acted_turns.contains(&acted) {
                self.acted_turns.push_back(acted);
                if self.acted_turns.len() > STARTED_REFS {
                    self.acted_turns.pop_front();
                }
            }
        }
        let Some(s) = self
            .sent
            .as_mut()
            .filter(|s| s.thread.as_deref() == Some(ev.thread.as_str()))
        else {
            return;
        };
        let this_turn = s.turn.is_some() && ev.turn == s.turn;
        match &ev.body {
            EventBody::TurnStarted(p) if s.msg_ref.is_some() && message_ref(p) == s.msg_ref => {
                s.turn = ev.turn;
            }
            EventBody::TurnEnded(_) if this_turn => s.ended = true,
            _ if this_turn && acts => s.acted = true,
            _ => {}
        }
    }

    /// What the hub answered to taking back a message (`message.withdraw`; `failed` when the
    /// call failed), matched by its ref ([`AppState::withdrawals`]), whatever was sent since:
    /// taken back, its text goes back in the input box when that is empty and shows its
    /// thread, else it is the newest history entry; too late, the status bar says why.
    fn withdraw_result(&mut self, result: &Value) {
        let msg_ref = result["ref"].as_i64();
        let Some(i) = self
            .withdrawals
            .iter()
            .position(|w| Some(w.msg_ref) == msg_ref)
        else {
            return;
        };
        let mut w = self.withdrawals.remove(i);
        self.queue_changed(&w.thread);
        // A suggestion whose send was answered after the withdrawal began (the queue's list can
        // come first): its answer, ahead of this one on the same calls, registered it.
        let known = self
            .suggested_refs
            .iter()
            .position(|(t, r)| *t == w.thread && *r == w.msg_ref);
        w.suggested |= known.is_some();
        if let (Some(k), true) = (known, result["withdrawn"] == true) {
            self.suggested_refs.remove(k);
        }
        let last = self
            .sent
            .as_mut()
            .filter(|s| s.msg_ref == Some(w.msg_ref) && s.thread.as_deref() == Some(&w.thread));
        if result["failed"] == true {
            if let Some(s) = last {
                s.withdrawing = false;
            }
            return;
        }
        if result["withdrawn"] == true {
            if last.is_some() {
                self.sent = None;
            }
            let here = Some(&w.thread) == self.main_thread().as_ref();
            // A queued message comes back whole from the hub: the list had a preview.
            // The hub's text is the message as sent: back in the box or the history as the line
            // that sends it again, never a command.
            let text = match (w.purpose, result["prompt"].as_str()) {
                (Retract::Edit | Retract::Delete, Some(full)) => literal_line(full),
                (Retract::Edit | Retract::Delete, None) => literal_line(&w.text),
                _ => w.text.clone(),
            };
            // Someone else's message (the orchestrator's, a `!!` command's output): gone, and
            // nowhere it could be sent again as the user's.
            if !w.mine {
                self.set_timed_status("deleted from the queue".to_string(), Instant::now());
                return;
            }
            // A suggestion: in the box it is still one; never in the history.
            if w.suggested && !(here && self.view.input.is_empty() && w.purpose != Retract::Delete)
            {
                self.set_timed_status(
                    "taken back: it was a suggestion, so it is not kept in the history".into(),
                    Instant::now(),
                );
                return;
            }
            if w.purpose == Retract::Delete {
                self.keep_taken_back(&text);
                self.set_timed_status(
                    "deleted from the queue: its text is in the history (↑)".to_string(),
                    Instant::now(),
                );
                return;
            }
            let msg = if here && self.view.input.is_empty() {
                let (cols, rows) = (self.input_cols(), self.input_rows());
                self.view.input.set(&text);
                self.view.input.keep_visible(cols, rows);
                if w.suggested {
                    self.taken_suggestion = Some(text.clone());
                }
                if w.purpose == Retract::Edit {
                    "taken back from the queue: edit it, Enter sends it again (at the queue's end)"
                } else {
                    "taken back: your message is in the box again"
                }
            } else {
                self.keep_taken_back(&text);
                if here {
                    "taken back: the box wasn't empty, so the message is in the history (↑)"
                } else {
                    "taken back: the message is in the history (↑)"
                }
            };
            self.set_timed_status(msg.to_string(), Instant::now());
            return;
        }
        let why = match result["reason"].as_str() {
            Some("acted" | "ended") => NoUnsend::TooLate,
            Some("uninterruptible") => NoUnsend::Uninterruptible,
            _ => NoUnsend::Nothing,
        };
        if let Some(s) = last {
            s.withdrawing = false;
            s.refused = Some(why);
        }
        let msg = self.no_unsend(why, Some(&w.thread));
        self.set_timed_status(msg, Instant::now());
    }

    /// Makes `text`, a message taken back that could not go back in the box, the newest
    /// history entry (`↑` recalls it), whatever its size: one too long for the history file
    /// is kept for this session only.
    pub(crate) fn keep_taken_back(&mut self, text: &str) {
        let v = &mut self.view;
        v.history_pos = None;
        if v.history.last().map(String::as_str) == Some(text) {
            return;
        }
        v.history.push(text.to_string());
        let over = v.history.len().saturating_sub(HISTORY_MAX);
        v.history.drain(..over);
        if text.len() <= crate::HISTORY_ENTRY_MAX {
            v.history_new.push(text.to_string());
        }
    }

    /// What Enter on an empty line opens the action popup on: the request selected in the
    /// conversation, else the selected card or tile when the current view draws it, else, in
    /// a dive with nothing selected, the thread dived into ([`AppState::selected_card`]: the
    /// popup draws its item itself). None while the main pane is selected, or for a request
    /// no longer open: never another item in its place.
    pub fn selection_target(&self) -> Option<ActionTarget> {
        if let Some(t) = self.queued_target() {
            return Some(t);
        }
        if let Some(i) = self.selected_request() {
            return Some(ActionTarget::Request(i.attn_id.clone()));
        }
        if self.view.request.is_some() || self.main_focused() {
            return None;
        }
        let card = self.selected_card()?;
        // A compact layout's list: only its rows.
        let unlisted = !self.compact_listed()
            && (self.view.dive.as_deref() == Some(card) || self.detail_card() == Some(card));
        (unlisted || self.nav_order().iter().any(|id| id == card))
            .then(|| ActionTarget::Card(card.to_string()))
    }

    /// The card the selection's item and action popup are about: the selected one, else, in a
    /// dive with nothing selected (the main pane neither), the thread dived into, so its own
    /// request is answered without selecting its card first.
    pub fn selected_card(&self) -> Option<&str> {
        self.focused().or_else(|| {
            self.view.dive.as_deref().filter(|_| {
                self.view.screen == Screen::Stack && !self.main_focused() && !self.compact_listed()
            })
        })
    }

    /// The action popup, while it is open over what is still selected, and nothing else
    /// modal is on screen.
    pub fn action_popup(&self) -> Option<&ActionPopup> {
        if self.picker_open() || self.view.help || self.view.trash.is_some() {
            return None;
        }
        let popup = self.view.action.as_ref()?;
        (self.selection_target().as_ref() == Some(&popup.target)).then_some(popup)
    }

    /// Opens the action popup on `target`, the selection: its first row highlighted (Talk to
    /// it, where there is one, so Enter, Enter dives into a worker). The item it shows counts
    /// as on screen from the popup's first frame on ([`ViewState::shown`] starts again), and
    /// as read already when the last frame drew it in full ([`ActionPopup::read`]).
    fn open_action(&mut self, target: ActionTarget) {
        let read = self
            .selected_attention()
            .map(|i| (i.attn_id.clone(), i.revision))
            .filter(|(id, rev)| {
                self.view
                    .shown
                    .as_ref()
                    .is_some_and(|s| s.attn_id == *id && s.revision == *rev)
            });
        // Talk to it, or a queued message's edit: rows Enter can run safely.
        let rows = self.action_rows(&target);
        let sel = [ActionKind::Talk, ActionKind::Edit]
            .into_iter()
            .find(|k| rows.iter().any(|r| r.kind == *k));
        self.view.shown = None;
        self.view.status = None;
        self.view.action = Some(ActionPopup {
            target,
            sel,
            filter: String::new(),
            read,
            armed: None,
        });
    }

    /// The selection moved while the action popup was open (Tab, Shift-Tab, a Shift-arrow):
    /// the popup shows the newly selected card's, or closes when nothing it opens on is
    /// selected now.
    fn follow_action(&mut self) {
        if self.view.action.is_none() {
            return;
        }
        match self.selection_target() {
            Some(t) if self.view.action.as_ref().is_some_and(|p| p.target == t) => {}
            Some(t) => self.open_action(t),
            None => self.view.action = None,
        }
    }

    /// Closes the action popup when what it was opened on is no longer the selection (another
    /// card clicked, the card gone, the request resolved, another view): it never goes over
    /// to something the user did not move it to.
    pub fn sync_action(&mut self) {
        if self.view.action.is_some() && self.action_popup().is_none() {
            self.view.action = None;
        }
    }

    /// The rows of the action popup on `target` (the selection, whose item
    /// [`AppState::selected_attention`] is): the item's options, numbered; then, for a
    /// worker's card, Talk to it, Dismiss (an item the hub raised), Pin or Unpin, Stop, Park or
    /// Resume and, with nothing waiting, Attach; for the hub's card and the orchestrator's
    /// banner, Dismiss where the item allows it; for a request, Talk to its thread.
    pub fn action_rows(&self, target: &ActionTarget) -> Vec<ActionRow> {
        let item = self.selected_attention();
        let mut rows: Vec<ActionRow> = item
            .map(|i| {
                i.options
                    .iter()
                    .enumerate()
                    .map(|(n, opt)| ActionRow {
                        kind: ActionKind::Answer(n + 1),
                        label: if n < 9 {
                            format!("{} {opt}", n + 1)
                        } else {
                            format!("{}. {opt}", n + 1)
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        let row = |kind, label: &str| ActionRow {
            kind,
            label: label.to_string(),
        };
        let dismissable = item.is_some_and(|i| i.thread == HUB_THREAD || !i.reason.is_blocking());
        match target {
            ActionTarget::Queued(r) => {
                let q = self.shown_queue();
                let it = q.and_then(|q| q.items.iter().find(|i| i.msg_ref == *r));
                if let Some(it) = it {
                    let name = q
                        .and_then(|q| self.registry.get(&q.thread))
                        .map_or("the thread".to_string(), |t| t.name.clone());
                    if it.mine {
                        rows.push(row(ActionKind::Edit, "edit: back into the box"));
                    }
                    rows.push(row(ActionKind::Delete, "delete"));
                    if q.and_then(|q| q.items.first()).map(|f| f.msg_ref) == Some(*r) {
                        rows.push(row(
                            ActionKind::SendNow,
                            &format!("send now: interrupts {name}'s turn"),
                        ));
                    }
                }
            }
            ActionTarget::Request(_) => {
                if let Some(t) = item.and_then(|i| self.registry.get(&i.thread)) {
                    rows.push(row(ActionKind::Talk, &format!("talk to {}", t.name)));
                }
                if dismissable {
                    rows.push(row(ActionKind::Dismiss, "dismiss"));
                }
            }
            ActionTarget::Card(id) => {
                let worker = self.registry.get(id).filter(|t| !t.is_orchestrator());
                // Already talking to it in a dive: no Talk row.
                if let Some(t) = worker.filter(|t| self.view.dive.as_ref() != Some(&t.id)) {
                    rows.push(row(ActionKind::Talk, &format!("talk to {}", t.name)));
                }
                if dismissable {
                    rows.push(row(ActionKind::Dismiss, "dismiss"));
                }
                if let Some(t) = worker {
                    let pinned = self.view.pins.contains(&t.id);
                    rows.push(row(ActionKind::Pin, if pinned { "unpin" } else { "pin" }));
                    rows.push(row(ActionKind::Stop, "stop"));
                    rows.push(if t.parked {
                        row(ActionKind::Resume, "resume")
                    } else {
                        row(ActionKind::Park, "park")
                    });
                    if item.is_none() {
                        rows.push(row(ActionKind::Attach, "attach"));
                    }
                }
            }
        }
        rows
    }

    /// Indices of the action popup's rows its filter keeps (every row without one).
    pub fn action_matches(&self, popup: &ActionPopup, rows: &[ActionRow]) -> Vec<usize> {
        let filter = popup.filter.to_lowercase();
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.label.to_lowercase().contains(&filter))
            .map(|(i, _)| i)
            .collect()
    }

    /// Keys of the open action popup: 1-9 answer, ↑↓ (Ctrl-P/N) move between the rows the
    /// filter keeps, Enter runs the highlighted one, letters filter, Backspace takes the last
    /// letter back, Esc closes it; Shift-Enter types nothing. The keys that scroll the main
    /// pane (PgUp/PgDn, Ctrl-↑/↓ and Alt-↑/↓) close it and scroll nothing: it covers part of
    /// the pane (an item too long for it, the detail view it opened), which they then scroll.
    /// `None` for the keys it leaves to the selection (Tab, Shift-Tab, the Shift-arrows) and
    /// the view (Ctrl-B, Ctrl-F, Ctrl-O, Ctrl-G); the rest of the keys do nothing while it is
    /// open.
    fn action_key(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        let popup = self.action_popup()?.clone();
        let rows = self.action_rows(&popup.target);
        let matches = self.action_matches(&popup, &rows);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let at_row = popup
            .sel
            .and_then(|k| rows.iter().position(|r| r.kind == k));
        let at = at_row.and_then(|r| matches.iter().position(|&i| i == r));
        // The next (or previous) row the filter keeps, wrapping around; with none highlighted,
        // the first (or the last).
        let step = |down: bool| -> Option<usize> {
            let n = matches.len();
            (n > 0).then(|| match at {
                Some(i) if down => matches[(i + 1) % n],
                Some(i) => matches[(i + n - 1) % n],
                None if down => matches[0],
                None => matches[n - 1],
            })
        };
        let moved = match key.code {
            KeyCode::Up | KeyCode::Down if !shift && !ctrl && !alt => {
                Some(step(key.code == KeyCode::Down))
            }
            KeyCode::Char('p' | 'n') if ctrl => Some(step(key.code == KeyCode::Char('n'))),
            _ => None,
        };
        if let Some(sel) = moved {
            if let (Some(sel), Some(p)) = (sel, self.view.action.as_mut()) {
                p.sel = Some(rows[sel].kind);
            }
            return Some(Vec::new());
        }
        match key.code {
            KeyCode::Esc => self.view.action = None,
            KeyCode::Char(c @ '1'..='9') if !ctrl && !alt => {
                let n = c as usize - '0' as usize;
                // An item the popup counts as read from its detail view though it lists only
                // some of the options ([`Panes::popup_partial`]): only an option it drew.
                let listed = self
                    .view
                    .panes
                    .popup_rows
                    .as_ref()
                    .is_some_and(|(t, kinds)| {
                        *t == popup.target && kinds.contains(&ActionKind::Answer(n))
                    });
                if self.view.panes.popup_partial
                    && !listed
                    && rows.iter().any(|r| r.kind == ActionKind::Answer(n))
                {
                    self.view.status = Some(format!(
                        "option {n} is not on screen: pick it with ↑↓, then Enter"
                    ));
                    return Some(Vec::new());
                }
                return Some(self.run_action(ActionKind::Answer(n)));
            }
            KeyCode::Enter if !shift && !alt && !ctrl => {
                // Only a row the last frame drew in this popup: never one clipped off a short
                // screen, or of a popup not drawn yet.
                let drawn = |k: ActionKind| {
                    self.view
                        .panes
                        .popup_rows
                        .as_ref()
                        .is_some_and(|(t, kinds)| *t == popup.target && kinds.contains(&k))
                };
                let row = at_row.filter(|_| at.is_some()).map(|r| rows[r].kind);
                return Some(match row {
                    Some(k) if drawn(k) => self.run_action(k),
                    Some(_) => {
                        self.view.status =
                            Some("that row is not on screen: pick one with ↑↓".into());
                        Vec::new()
                    }
                    None if matches.is_empty() => {
                        self.view.status = Some(format!("no row has {}", popup.filter));
                        Vec::new()
                    }
                    None => {
                        self.view.status =
                            Some("press an option's number, or pick a row with ↑↓".into());
                        Vec::new()
                    }
                });
            }
            // With no filter, Backspace takes nothing back and changes nothing.
            KeyCode::Backspace if !popup.filter.is_empty() => {
                let mut filter = popup.filter.clone();
                filter.pop();
                self.filter_action(filter, &rows);
            }
            KeyCode::Backspace => {}
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut filter = popup.filter.clone();
                filter.push(c);
                self.filter_action(filter, &rows);
            }
            KeyCode::Tab | KeyCode::BackTab => return None,
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right if shift => return None,
            KeyCode::PageUp | KeyCode::PageDown => self.view.action = None,
            KeyCode::Up | KeyCode::Down if ctrl || alt => self.view.action = None,
            _ if ctrl || alt => return None,
            _ => {}
        }
        Some(Vec::new())
    }

    /// Sets the action popup's filter to `filter`. The highlight goes to a row on its own only
    /// when the filter keeps that row alone (Enter then runs it); otherwise it stays on its row
    /// while the filter keeps that, and is dropped when not: a stray letter never highlights
    /// an answer that Enter would send.
    fn filter_action(&mut self, filter: String, rows: &[ActionRow]) {
        let Some(mut popup) = self.view.action.take() else {
            return;
        };
        popup.filter = filter;
        let matches = self.action_matches(&popup, rows);
        popup.sel = match matches.as_slice() {
            [only] => Some(rows[*only].kind),
            _ => popup
                .sel
                .filter(|k| matches.iter().any(|&i| rows[i].kind == *k)),
        };
        self.view.action = Some(popup);
    }

    /// Runs an action popup row on its target. An answer or a dismissal the guards refuse
    /// (not on screen in full, just changed: [`AppState::answer`]) leaves the popup open with
    /// the status saying why; anything else closes it.
    fn run_action(&mut self, kind: ActionKind) -> Vec<Action> {
        let Some(target) = self.view.action.as_ref().map(|p| p.target.clone()) else {
            return Vec::new();
        };
        if let ActionTarget::Queued(r) = target {
            self.view.action = None;
            return match kind {
                ActionKind::Edit => self.retract_queued(r, Retract::Edit),
                ActionKind::Delete => self.retract_queued(r, Retract::Delete),
                ActionKind::SendNow => self.send_queued_now(r),
                _ => Vec::new(),
            };
        }
        let thread = match &target {
            ActionTarget::Card(id) => Some(id.clone()),
            ActionTarget::Request(_) => self.selected_attention().map(|i| i.thread.clone()),
            ActionTarget::Queued(_) => None,
        };
        match kind {
            ActionKind::Answer(n) => {
                let actions = self.answer(n);
                if !actions.is_empty() {
                    self.view.action = None;
                }
                return actions;
            }
            ActionKind::Dismiss => {
                let actions = self.dismiss();
                if !actions.is_empty() {
                    self.view.action = None;
                }
                return actions;
            }
            _ => self.view.action = None,
        }
        let Some(thread) = thread.filter(|t| self.registry.get(t).is_some()) else {
            return Vec::new();
        };
        match kind {
            ActionKind::Talk => self.dive(&thread),
            ActionKind::Pin => {
                if let Err(e) = self.toggle_pin(Some(&thread)) {
                    self.view.status = Some(e);
                }
            }
            ActionKind::Stop => return vec![Action::Stop { thread }],
            ActionKind::Park | ActionKind::Resume => {
                self.confirm_lifecycle_of(kind == ActionKind::Park, Some(&thread));
            }
            ActionKind::Attach => {
                let name = self
                    .registry
                    .get(&thread)
                    .map_or(thread.clone(), |t| t.name.clone());
                self.view.status = Some(format!(
                    "to attach, run in another terminal: twr{} attach {name}",
                    self.attach_via
                ));
            }
            ActionKind::Answer(_)
            | ActionKind::Dismiss
            | ActionKind::Edit
            | ActionKind::Delete
            | ActionKind::SendNow => {}
        }
        Vec::new()
    }

    /// The popup's suggestions for the input line, when it is open: typing a `/` line or an
    /// `@` mention ([`slash::mention`]), not closed with Esc on this line, and something
    /// matches.
    pub fn popup(&self) -> Option<slash::Suggestions> {
        // A recalled line shows no popup while Up and Down still browse the history. The
        // decisions overlay has no input box, and the action popup takes the keys.
        if self.view.screen == Screen::Decisions
            || self.view.action.is_some()
            || self.picker_open()
            || self.view.help
            || self.browsing().is_some()
        {
            return None;
        }
        let line = self.view.input.as_str();
        if self.view.popup_closed.as_deref() == Some(line) {
            return None;
        }
        let pools = self.pools();
        let mut s = slash::suggest(line, &pools);
        // A `!` command is no message: its `@` is the shell's.
        if s.items.is_empty() && !line.trim_start().starts_with('!') {
            s = slash::mention(line, self.view.input.cursor(), &pools);
        }
        (!s.items.is_empty()).then_some(s)
    }

    /// The popup's selected row, clamped to `n` rows.
    pub fn popup_selected(&self, n: usize) -> usize {
        self.view.popup_sel.min(n.saturating_sub(1))
    }

    /// What `/` line arguments complete to: threads (not retired) with their status,
    /// runtimes with whether they are installed; and the worker threads `@` mentions name.
    fn pools(&self) -> slash::Pools {
        let detail = |t: &ThreadView| {
            let rt = if t.runtime.is_empty() {
                "?"
            } else {
                &t.runtime
            };
            format!("{} · {rt} · mode {}", t.status().as_str(), t.mode)
        };
        let workers: Vec<&ThreadView> = self
            .registry
            .list()
            .into_iter()
            .filter(|t| !t.retired && !t.is_orchestrator())
            .collect();
        let threads = workers
            .iter()
            .map(|t| (t.name.clone(), detail(t)))
            .collect();
        // The registry never lists the hub; it is kept out here all the same, as it is never
        // a relay target.
        let mentions = workers
            .iter()
            .filter(|t| t.id != HUB_THREAD)
            .map(|t| slash::Mentionable {
                name: t.name.clone(),
                aliases: t.aliases.clone(),
                detail: detail(t),
                // A parked thread, listed or hidden, is still named by an `@` mention, after
                // the live ones like a stopped one.
                live: !t.stopped && !t.parked,
            })
            .collect();
        let runtimes = self
            .runtimes()
            .into_iter()
            .map(|r| {
                let installed = self
                    .view
                    .orch_installed
                    .iter()
                    .find(|(n, _)| *n == r)
                    .map(|(_, i)| *i);
                let detail = match installed {
                    Some(true) => "installed",
                    Some(false) => "not installed",
                    None => "",
                };
                (r, detail.to_string())
            })
            .collect();
        let workspaces = self
            .tabs
            .iter()
            .map(|t| {
                let current = self.workspace.as_deref() == Some(t.name.as_str());
                (
                    t.name.clone(),
                    if current { "current" } else { "" }.to_string(),
                )
            })
            .collect();
        slash::Pools {
            runtimes,
            threads,
            dive: self.view.dive.is_some(),
            mentions,
            workspaces,
        }
    }

    /// Keys of the open popup: Up/Down (Ctrl-P/N) move, Tab completes the selected row, Enter
    /// on a partial command name completes it (and runs it when it takes no arguments), Enter
    /// on a mention puts the selected thread in and sends nothing (unless the mention's whole
    /// word, the cursor wherever in it, is already a thread name at the end of the line: then
    /// it sends), Esc closes the popup. `None`: the key is not the popup's.
    fn popup_key(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        let s = self.popup()?;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let n = s.items.len();
        let sel = self.popup_selected(n);
        match key.code {
            // Up and Down alone: with Shift they move the selection, with Ctrl or Alt they
            // scroll the main pane, popup or not.
            KeyCode::Up if key.modifiers.is_empty() => self.view.popup_sel = (sel + n - 1) % n,
            KeyCode::Char('p') if ctrl => self.view.popup_sel = (sel + n - 1) % n,
            KeyCode::Down if key.modifiers.is_empty() => self.view.popup_sel = (sel + 1) % n,
            KeyCode::Char('n') if ctrl => self.view.popup_sel = (sel + 1) % n,
            KeyCode::Esc => {
                self.view.popup_closed = Some(self.view.input.as_str().to_string());
            }
            KeyCode::Tab => self.popup_complete(&s, sel),
            KeyCode::Enter if s.mention => {
                let line = self.view.input.as_str();
                // The whole word, also what follows the cursor in it (what Enter would replace).
                let typed = &line[s.start..s.end];
                if line[s.end..].trim().is_empty() && s.items.iter().any(|i| i.value == typed) {
                    // A whole thread name at the end of the line: send it as typed.
                    return None;
                }
                self.popup_complete(&s, sel);
            }
            KeyCode::Enter if s.command => {
                let word = self.view.input.as_str().trim_end();
                if slash::canonical(word).is_some() {
                    // A whole command name: run it as typed.
                    return None;
                }
                let item = &s.items[sel];
                self.popup_complete(&s, sel);
                if item.more {
                    return Some(Vec::new());
                }
                return None;
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// Puts row `sel` of `s` into the input line in place of the word it completes, with a
    /// space after it when typing goes on (the one already after it, stepped over, when there
    /// is one). The cursor goes after it.
    fn popup_complete(&mut self, s: &slash::Suggestions, sel: usize) {
        let item = &s.items[sel];
        let spaced = self.view.input[s.end..].starts_with(char::is_whitespace);
        let mut value = item.value.clone();
        if item.more && !spaced {
            value.push(' ');
        }
        self.view.input.replace(s.start, s.end, &value);
        if item.more && spaced {
            self.view.input.right();
        }
        self.view
            .input
            .keep_visible(self.input_cols(), self.input_rows());
        self.view.popup_sel = 0;
        self.view.status = None;
    }

    /// Keys of the orchestrator picker: Up/Down (or k/j) and 1-9 pick a runtime, Enter chooses
    /// it (the first-run choice at once, a switch once confirmed), Esc leaves the choice for
    /// later (a switch: for good). Anything else only says so: the picker is modal, nothing
    /// typed meanwhile goes anywhere.
    fn picker_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let Some(p) = self.view.picker.as_mut() else {
            return Vec::new();
        };
        let n = p.runtimes.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => p.selected = p.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => p.selected = (p.selected + 1).min(n - 1),
            KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n => {
                p.selected = c as usize - '1' as usize;
            }
            KeyCode::Enter if p.switch => {
                let runtime = p.runtimes[p.selected].0.clone();
                self.view.picker = None;
                self.confirm_switch(runtime, None);
            }
            KeyCode::Esc if p.switch => {
                self.view.picker = None;
                self.view.status = None;
            }
            KeyCode::Enter => {
                let runtime = p.runtimes[p.selected].0.clone();
                let model = p.model.clone().filter(|_| runtime == p.default);
                self.view.picker = None;
                self.view.status = Some(format!(
                    "choosing {runtime} for the orchestrator: waiting for the hub"
                ));
                return vec![Action::UseOrchestrator {
                    runtime,
                    model,
                    summary: false,
                }];
            }
            KeyCode::Esc => {
                let default = p.default.clone();
                self.view.picker = None;
                self.view.status = Some(format!(
                    "no choice made: the orchestrator will be {default} (asked again next \
                     start; /orch switches it)"
                ));
            }
            _ if p.switch => {
                self.view.status = Some(
                    "pick the orchestrator's agent: ↑↓ or 1-9, Enter to choose, Esc to cancel"
                        .into(),
                );
            }
            _ => {
                self.view.status = Some(
                    "pick the orchestrator's agent: ↑↓ or 1-9, Enter to choose, Esc to decide \
                     later"
                        .into(),
                );
            }
        }
        Vec::new()
    }

    /// Columns of the input line's box (as last drawn; while it is not drawn, from the
    /// terminal width, [`crate::ui::input_box_width`]).
    pub(crate) fn input_cols(&self) -> usize {
        usize::from(self.view.panes.get(&PaneId::Input).map_or_else(
            || crate::ui::input_box_width(self, self.view.width).saturating_sub(2),
            |p| p.area.width,
        ))
    }

    /// Rows the input line's box may grow to at the terminal height of the last draw.
    pub(crate) fn input_rows(&self) -> usize {
        crate::ui::input_cap(self.view.height)
    }

    /// Breaks the input line at the cursor when `key` is a line break, and says whether it
    /// was: Shift-Enter (told apart from Enter where the terminal reports it, as with the
    /// keyboard protocol [`crate::run_with`] asks for), or Alt-Enter or Ctrl-J (wherever
    /// Shift-Enter arrives as a plain Enter). A plain Enter always sends.
    fn newline(&mut self, key: KeyEvent) -> bool {
        let m = key.modifiers;
        let newline = match key.code {
            KeyCode::Enter => m.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT),
            KeyCode::Char('j') => m == KeyModifiers::CONTROL,
            _ => false,
        };
        if !newline {
            return false;
        }
        if key.code == KeyCode::Enter && m.contains(KeyModifiers::SHIFT) {
            self.view.shift_enter = true;
        }
        self.view.input.newline();
        self.view
            .input
            .keep_visible(self.input_cols(), self.input_rows());
        self.view.popup_sel = 0;
        true
    }

    fn input_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let (cols, rows) = (self.input_cols(), self.input_rows());
        // Up on the first row and Down on the last browse the history; elsewhere they move
        // between the rows of the line.
        if key.modifiers.is_empty() {
            // With messages of the user's queued, ↑ on an empty line takes the newest back to
            // edit it ([`crate::queue`]); the history comes after.
            if key.code == KeyCode::Up {
                if let Some(actions) = self.edit_newest_queued() {
                    return actions;
                }
            }
            match key.code {
                KeyCode::Up if self.view.input.on_first_row(cols) && self.history_prev() => {
                    return Vec::new();
                }
                KeyCode::Down if self.browsing().is_some() && self.view.input.on_last_row(cols) => {
                    self.history_next();
                    return Vec::new();
                }
                _ => {}
            }
        }
        if edit_key(&mut self.view.input, key, cols, rows) {
            self.view.popup_sel = 0;
            return Vec::new();
        }
        match key.code {
            KeyCode::Enter => {
                let text = self.view.input.trim().to_string();
                // Only blanks: nothing to send (an empty line's Enter is the selection's).
                if text.is_empty() {
                    return Vec::new();
                }
                // A suggestion taken and sent as it was: it goes as one, and never to the
                // history, from where it would come back as the user's own words.
                let suggested = self.taken_suggestion.take().as_deref() == Some(text.as_str());
                if !suggested {
                    self.remember(&text);
                }
                // `!...` runs a shell command ([`crate::shell`]); `\!...` sends `!...`.
                if text.starts_with('!') {
                    let actions = self.shell_line(&text);
                    if !actions.is_empty() {
                        self.view.input.clear();
                        self.follow();
                    }
                    return actions;
                }
                // `//...` sends `/...`; any other `/...` is a command, never a message.
                if !text.starts_with("//") && text.starts_with('/') {
                    let (actions, done) = self.run_slash(&text);
                    if done {
                        self.view.input.clear();
                    }
                    return actions;
                }
                // The line as typed is what taking it back puts in the box again: escapes
                // and all, so it is the same message, never a command.
                let typed = text.clone();
                let text = message_text(&text);
                // Only a dive sends to a thread; everything else goes to the orchestrator.
                // In a dive too: taken back, it is still a suggestion where it goes next.
                self.note_sent(self.view.dive.clone(), &typed, suggested);
                let action = match &self.view.dive {
                    Some(thread) => Action::Send {
                        thread: thread.clone(),
                        text,
                    },
                    // A suggestion taken and sent as it was: the hub never applies it as a short
                    // command at once.
                    None if suggested => Action::SaySuggested {
                        text,
                        seen_gseq: self.seen_gseq(),
                    },
                    None => Action::Say {
                        text,
                        seen_gseq: self.seen_gseq(),
                    },
                };
                self.view.input.clear();
                self.follow();
                vec![action]
            }
            _ => Vec::new(),
        }
    }

    /// Adds `text`, sent or run from the input line, to the history (not again right after
    /// itself), and stops browsing it.
    fn remember(&mut self, text: &str) {
        let v = &mut self.view;
        v.history_pos = None;
        v.history_draft.clear();
        if v.history.last().map(String::as_str) == Some(text)
            || text.len() > crate::HISTORY_ENTRY_MAX
        {
            return;
        }
        v.history.push(text.to_string());
        let over = v.history.len().saturating_sub(HISTORY_MAX);
        v.history.drain(..over);
        v.history_new.push(text.to_string());
    }

    /// Puts the history from an earlier session first (oldest first), keeping the newest
    /// [`HISTORY_MAX`] entries.
    pub fn load_history(&mut self, mut earlier: Vec<String>) {
        earlier.append(&mut self.view.history);
        let over = earlier.len().saturating_sub(HISTORY_MAX);
        earlier.drain(..over);
        self.view.history = earlier;
    }

    /// The history entry the input line shows, while browsing: the one last recalled, as
    /// long as the line has not changed since. An edited line is the line being typed.
    pub fn browsing(&self) -> Option<usize> {
        let v = &self.view;
        v.history_pos
            .filter(|&p| v.history.get(p).is_some_and(|h| v.input == *h.as_str()))
    }

    /// Up on the input line's first row: the entry before the one shown (the newest when not
    /// browsing, keeping what was typed). Returns whether there was one to show.
    pub fn history_prev(&mut self) -> bool {
        let browsing = self.browsing();
        let v = &mut self.view;
        let pos = match browsing {
            _ if v.history.is_empty() => return false,
            None => {
                v.history_draft = v.input.as_str().to_string();
                v.history.len() - 1
            }
            Some(0) => return true,
            Some(p) => p - 1,
        };
        v.history_pos = Some(pos);
        let text = v.history[pos].clone();
        self.show_in_input(&text);
        true
    }

    /// Down on the input line's last row while browsing: the next entry, or past the newest
    /// what was typed before browsing.
    pub fn history_next(&mut self) {
        let Some(p) = self.browsing() else {
            return;
        };
        let v = &mut self.view;
        let text = if p + 1 < v.history.len() {
            v.history_pos = Some(p + 1);
            v.history[p + 1].clone()
        } else {
            v.history_pos = None;
            std::mem::take(&mut v.history_draft)
        };
        self.show_in_input(&text);
    }

    /// Puts `text` on the input line, the cursor at its end.
    fn show_in_input(&mut self, text: &str) {
        let (cols, rows) = (self.input_cols(), self.input_rows());
        self.view.input.set(text);
        self.view.input.keep_visible(cols, rows);
        self.view.popup_sel = 0;
    }

    /// Keys of the decisions overlay: ↑↓ (or k/j) move its selection, PgUp/PgDn a page,
    /// Home/End to its ends, `f` and `v` cycle its site and provider filters, Enter opens the
    /// selected decision's thread, `?` the help, Esc closes it. `None` for the keys about the
    /// view it leaves to [`AppState::view_key`] (Ctrl-B, Ctrl-F, Ctrl-O); any other key does
    /// nothing: it has no input box.
    fn decisions_key(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('b' | 'f' | 'o') if ctrl => return None,
            _ if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {}
            KeyCode::Esc => self.toggle_decisions(),
            KeyCode::Down | KeyCode::Char('j') => self.move_decision(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_decision(-1),
            KeyCode::PageDown => self.move_decision(SCROLL_STEP as isize),
            KeyCode::PageUp => self.move_decision(-(SCROLL_STEP as isize)),
            KeyCode::Home => self.view.decision_sel = 0,
            KeyCode::End => {
                self.view.decision_sel = self.decision_rows().len().saturating_sub(1);
            }
            KeyCode::Char('f') => {
                self.view.decision_filter.cycle_site();
                self.view.decision_sel = 0;
            }
            KeyCode::Char('v') => {
                self.view.decision_filter.cycle_provider();
                self.view.decision_sel = 0;
            }
            KeyCode::Enter => self.open_decision(),
            KeyCode::Char('?') => self.open_help(),
            _ => {}
        }
        Some(Vec::new())
    }

    /// The long block a toggle acts on: the selected one, else the last visible one.
    fn block_target(&self, view: &crate::ui::BlockView) -> Option<String> {
        if let Some(key) = self
            .view
            .block
            .as_deref()
            .filter(|k| view.block(k).is_some())
        {
            return Some(key.to_string());
        }
        self.view
            .panes
            .get(&PaneId::Transcript)?
            .links
            .iter()
            .rev()
            .filter_map(|l| l.as_deref().and_then(crate::ui::linked_block))
            .find(|key| view.block(key).is_some())
            .map(str::to_string)
    }

    /// Shift-↑ / Shift-↓ with the main pane selected: selects the previous or next step of
    /// its conversation, a long block or a request, in the order they are drawn, stopping at
    /// the first and the last. With neither selected, Shift-↑ selects the last step that
    /// starts above the pane's bottom as last drawn and Shift-↓ the first that starts at or
    /// below its top (else the last one). A block is shown from its head; a request is kept
    /// in view ([`AppState::select_request`]).
    fn step_main(&mut self, previous: bool) {
        if self.view.screen != Screen::Stack {
            return;
        }
        // The queued messages listed under the conversation are its last steps: Shift-↓ goes
        // on into them, Shift-↑ back up out of them.
        let queued: Vec<i64> = self
            .shown_queue()
            .map(|q| {
                q.items
                    .iter()
                    .take(crate::queue::QUEUE_ROWS)
                    .map(|i| i.msg_ref)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(i) = self
            .view
            .queued
            .and_then(|cur| queued.iter().position(|r| *r == cur))
        {
            match (previous, i) {
                (false, i) => {
                    if let Some(next) = queued.get(i + 1) {
                        self.select_queued(*next);
                    }
                    return;
                }
                (true, i) if i > 0 => {
                    self.select_queued(queued[i - 1]);
                    return;
                }
                // Up from the first: the conversation's last step.
                (true, _) => self.view.queued = None,
            }
        }
        let (key, head) = {
            let view = crate::ui::BlockView::new(self);
            let steps = view.steps();
            if steps.is_empty() {
                if let (false, Some(first)) = (previous, queued.first()) {
                    self.select_queued(*first);
                }
                return;
            }
            let last = steps.len() - 1;
            let current = match (&self.view.request, &self.view.block) {
                (Some(id), _) => Some(format!("request:{id}")),
                (None, Some(block)) => Some(block.clone()),
                (None, None) => None,
            };
            let at = current.and_then(|c| steps.iter().position(|(k, _)| *k == c));
            // Down from the last step: the first queued message.
            if let (false, Some(i), Some(first)) = (previous, at, queued.first()) {
                if i == last {
                    let first = *first;
                    drop(view);
                    self.select_queued(first);
                    return;
                }
            }
            let index = match at {
                Some(i) if previous => i.saturating_sub(1),
                Some(i) => (i + 1).min(last),
                None => {
                    let top = view.top().unwrap_or(0);
                    let height = self
                        .view
                        .panes
                        .get(&PaneId::Transcript)
                        .map_or(0, |p| usize::from(p.area.height));
                    if previous {
                        steps
                            .iter()
                            .rposition(|(_, row)| *row < top + height.max(1))
                            .unwrap_or(0)
                    } else {
                        steps
                            .iter()
                            .position(|(_, row)| *row >= top)
                            .unwrap_or(steps.len() - 1)
                    }
                }
            };
            let key = steps[index].0.clone();
            let head = view.block(&key).and_then(|rows| view.anchor(rows.start));
            (key, head)
        };
        if let Some(id) = key.strip_prefix("request:") {
            self.view.block = None;
            self.select_request(id);
            return;
        }
        self.release_request();
        if let Some(head) = head {
            self.scroll_to_anchor(head);
        }
        self.view.block = Some(key);
    }

    /// Expands or collapses long block `key` (else the last visible one, [`block_target`]) and
    /// selects it. With the main pane's top in it, the pane shows it from its head, so its
    /// first rows can be read; else the view stays on what it shows: on its anchor (where it
    /// is read, though collapsed away at this width), or a view following the bottom on the
    /// row at its top.
    fn toggle_block(&mut self, key: Option<String>) {
        if self.view.panes.card.is_some() {
            return;
        }
        let (key, top) = {
            let view = crate::ui::BlockView::new(self);
            let Some(key) = key.or_else(|| self.block_target(&view)) else {
                return;
            };
            let Some(rows) = view.block(&key) else {
                return;
            };
            let top = view
                .top()
                .filter(|top| rows.contains(top) || self.view.anchor.is_none())
                .map(|top| if rows.contains(&top) { rows.start } else { top })
                .and_then(|row| view.anchor(row));
            (key, top)
        };
        if !self.view.long_expanded.remove(&key) {
            self.view.long_expanded.insert(key.clone());
        }
        self.view.queued = None;
        self.view.block = Some(key);
        self.view.request = None;
        if let Some(top) = top {
            self.scroll_to_anchor(top);
        }
    }

    /// Whether the selected card is a worker thread's (one that can be dived into).
    fn focused_is_worker(&self) -> bool {
        self.focused_thread().is_some_and(|t| !t.is_orchestrator())
    }

    /// Dismisses the selected card's top item when the hub raised it (see
    /// [`Action::Dismiss`]); a runtime's permission or question must be answered (or the
    /// thread stopped), so this only says so. Like an answer, only an item that has been on
    /// screen in full for [`CARD_SETTLE`].
    fn dismiss(&mut self) -> Vec<Action> {
        let Some(item) = self.selected_attention() else {
            self.view.status = Some("nothing to dismiss on this thread".into());
            return Vec::new();
        };
        // Only the card on screen, once the user has had time to read it.
        let Some(s) = &self.view.shown else {
            self.view.status = Some(NOT_ON_SCREEN.into());
            return Vec::new();
        };
        let same = s.attn_id == item.attn_id && s.revision == item.revision;
        if !same || s.since.elapsed() < CARD_SETTLE {
            self.view.status = Some("the card just changed; read it and dismiss again".into());
            return Vec::new();
        }
        if item.thread != HUB_THREAD && item.reason.is_blocking() {
            self.view.status =
                Some("the runtime waits for this answer: answer it or stop the thread".into());
            return Vec::new();
        }
        let action = Action::Dismiss {
            attn_id: item.attn_id.clone(),
            revision: item.revision,
        };
        self.view.status = None;
        vec![action]
    }

    /// Scrolls the confirm card's detail view (as last drawn) down or up `n` rows, at most a
    /// page less a row, so no row is skipped (every row must have been on screen before its
    /// options answer it).
    fn scroll_card(&mut self, down: bool, n: usize) {
        let Some(w) = self.view.panes.card.as_ref() else {
            return;
        };
        let step = n.min(usize::from(w.page.saturating_sub(1))).max(1);
        let cv = &mut self.view.card_view;
        cv.scroll = if down {
            cv.scroll.saturating_add(step).min(w.max_scroll)
        } else {
            cv.scroll.min(w.max_scroll).saturating_sub(step)
        };
    }

    /// The main pane follows its transcript's bottom again: new rows show as they come.
    pub fn follow(&mut self) {
        self.view.scroll = 0;
        self.view.anchor = None;
    }

    /// Scrolls the main pane to the row `rows` rows below the first row of the entry `entry`
    /// (its id, [`Transcript::ids`]): the next frame draws that row at the top, or follows the
    /// bottom when less than a pane of rows is below it. A selected request still keeps
    /// itself in view.
    pub fn scroll_to(&mut self, entry: u64, rows: isize) {
        self.scroll_to_anchor(Anchor {
            entry: Some(entry),
            rows,
            ..Anchor::default()
        });
    }

    /// Scrolls the main pane to `anchor`'s place ([`Anchor`]): the next frame draws it at the
    /// top, or follows the bottom when less than a pane of rows is below it.
    pub fn scroll_to_anchor(&mut self, anchor: Anchor) {
        self.view.anchor = Some(anchor);
        self.view.scroll = self.view.scroll.max(1);
    }

    /// Keeps the panes `render` drew ([`crate::ui::render`]) and the transcript's scroll as
    /// that frame drew it: how far up from the bottom, and the row at its top, which a view
    /// scrolled up stays on ([`ViewState::anchor`]). A view drawn at the bottom follows it.
    /// A long block stays selected only while the transcript drawn has it ([`Pane::block`]),
    /// not while a card's detail view takes the main pane instead.
    ///
    /// The action popup without room for its item ([`Panes::long_item`]) opens the item's
    /// detail view in the main pane ([`ViewState::detail`]), showing the stack view for it.
    pub fn note_panes(&mut self, panes: Panes) {
        self.view.panes = panes;
        self.fit_compact();
        if let Some(card) = self.view.panes.long_item.take() {
            // First: the card stays selected in the stack view for its detail view, also with
            // the sidebar hidden, where the stack draws no worker card ([`AppState::ensure_focus`]).
            self.view.detail = Some(card);
            if self.view.screen == Screen::Grid {
                self.show(Screen::Stack);
            }
            // The compact layout's lists draw no main pane: the conversation screen does.
            if self.compact() {
                self.view.compact_pane = CompactPane::Chat;
            }
        }
        // The cards hidden by a dive (one that began wider, then the screen narrowed): the keys
        // about a card act on the thread the header names, not on one out of sight.
        // Not while the detail view of another card's item shows: it is that card's.
        if self.dive_hides_cards()
            && self.view.focus != self.view.dive
            && self.detail_card().is_none()
        {
            self.view.focus = self.view.dive.clone();
        }
        if let Some(p) = self.view.panes.get(&PaneId::Transcript) {
            self.view.scroll = p.scroll;
            self.view.anchor = p.anchor.clone().filter(|_| p.scroll > 0);
            self.view.block = p.block.clone();
        } else if self.view.panes.card.is_some() {
            self.view.block = None;
        }
    }

    /// The transcript row a scroll key moves from: the view's anchor (moved by keys since),
    /// else the top row as last drawn, even while it followed the bottom.
    fn scroll_base(&self) -> Option<Anchor> {
        let pane = self.view.panes.get(&PaneId::Transcript);
        self.view
            .anchor
            .clone()
            .or_else(|| pane.and_then(|p| p.top.clone()))
    }

    /// Scrolls the transcript up `n` lines, no further than its first line (as last drawn).
    fn scroll_up(&mut self, n: usize) {
        let pane = self.view.panes.get(&PaneId::Transcript);
        let max = pane.map_or(usize::MAX, |p| p.max_scroll);
        let scroll = self.view.scroll.saturating_add(n).min(max);
        let moved = scroll.saturating_sub(self.view.scroll.min(max));
        self.view.anchor = self.scroll_base().map(|a| Anchor {
            rows: a.rows.saturating_sub_unsigned(moved),
            ..a
        });
        self.view.scroll = scroll;
    }

    /// Scrolls the transcript to its first row: above it by more rows than there are (an
    /// anchor in no entry or card is at the top), which the next frame draws as its first
    /// row and anchors on.
    fn scroll_top(&mut self) {
        self.scroll_transcript(false, usize::MAX);
        if self.view.scroll > 0 {
            self.view.anchor = Some(Anchor {
                rows: -isize::MAX,
                ..Anchor::default()
            });
        }
    }

    /// Scrolls the transcript down `n` lines; at the bottom it follows it again.
    fn scroll_down(&mut self, n: usize) {
        let moved = n.min(self.view.scroll);
        self.view.scroll -= moved;
        self.view.anchor = self
            .scroll_base()
            .filter(|_| self.view.scroll > 0)
            .map(|a| Anchor {
                rows: a.rows.saturating_add_unsigned(moved),
                ..a
            });
    }

    /// Scrolls the transcript a line (`↑`/`↓` with the main pane selected). Up, also past the
    /// rows drawn only while it followed that are still in view (they go once it is scrolled,
    /// [`crate::select::Pane::follow_rows`]; a request held the view up to a few of them), so a
    /// line of text goes below the view.
    fn scroll_line(&mut self, down: bool) {
        // Where the view is: as drawn while a request held it (scrolling releases it, and
        // the view goes on from there), else as keys since left it.
        self.release_request();
        let follow_rows = self
            .view
            .panes
            .get(&PaneId::Transcript)
            .filter(|_| !down)
            .map_or(0, |p| p.follow_rows.saturating_sub(self.view.scroll));
        self.scroll_transcript(down, 1 + follow_rows);
    }

    /// Releases the inline request selected in the conversation, if any, leaving the view
    /// where it is drawn.
    fn release_request(&mut self) {
        if self.view.request.is_some() {
            if let Some(pane) = self.view.panes.get(&PaneId::Transcript) {
                self.view.scroll = pane.scroll;
                self.view.anchor = pane.anchor.clone().filter(|_| pane.scroll > 0);
            }
            self.view.request = None;
            self.view.shown = None;
        }
    }

    /// Release an inline request when the user explicitly scrolls the conversation.
    fn scroll_transcript(&mut self, down: bool, n: usize) {
        self.release_request();
        if down {
            self.scroll_down(n);
        } else {
            self.scroll_up(n);
        }
    }

    /// Handles a mouse event (the TUI has the mouse, [`ViewState::mouse`]). `screen` is the
    /// last drawn screen, where selected text is read from.
    ///
    /// A click (a tap: [`crate::select::is_tap`]) on a control the last frame drew
    /// ([`Target`]) does what it says ([`AppState::tap`]), when the button went down on that
    /// same control. Else a left-button drag selects inside the pane it starts in (never its
    /// border or another pane); on release the text is copied ([`Action::Copy`]) and stays
    /// highlighted until the next key or click. A click without a drag copies nothing
    /// ([`AppState::click`]): on a card, tile or compact row it selects that thread, and on the
    /// one selected it opens its action popup; in the conversation it selects the main pane, a
    /// request or a long block; on the input line it puts the cursor there. The wheel scrolls
    /// the pane under the pointer, or moves the highlight of the popup under it.
    pub fn handle_mouse(&mut self, ev: MouseEvent, screen: &Buffer) -> Vec<Action> {
        let view = self.armed_view();
        let armed = self.sent.as_ref().and_then(|s| s.armed);
        let actions = self.mouse(ev, screen);
        // What the click moved the selection to: the popup is about it, or closes.
        self.sync_action();
        self.disarm_on_view_change(&view, armed);
        actions
    }

    /// [`AppState::handle_mouse`], before the action popup is checked against the selection.
    fn mouse(&mut self, ev: MouseEvent, screen: &Buffer) -> Vec<Action> {
        // Over the cards, the pointer holds their order ([`AppState::cards_held`]).
        self.view.pointer = Some((ev.column, ev.row, Instant::now()));
        let (x, y) = (ev.column, ev.row);
        // The picker is modal: its runtimes and its ✕ are tapped, the wheel picks. Only a tab
        // of the strip, which it does not cover, switches workspaces.
        if self.picker_open() {
            match ev.kind {
                MouseEventKind::Down(MouseButton::Left) => self.press(x, y),
                MouseEventKind::Up(MouseButton::Left) => {
                    if let (_, Some(t)) = self.tapped(x, y) {
                        return self.tap(t);
                    }
                    if let Some(PaneId::Tab(name)) = self.view.panes.at(x, y).map(|p| p.id.clone())
                    {
                        return self.switch_workspace(&name);
                    }
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let code = if ev.kind == MouseEventKind::ScrollUp {
                        KeyCode::Up
                    } else {
                        KeyCode::Down
                    };
                    return self.picker_key(KeyEvent::new(code, KeyModifiers::NONE));
                }
                _ => {}
            }
            return Vec::new();
        }
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.view.selection = None;
                let pane = self.view.panes.at(x, y).cloned();
                let id = pane.as_ref().map(|p| p.id.clone());
                // Like a key, a click outside the help (or the trash: its `✕` is outside its
                // text too) closes it, and does nothing else; one on a tab of the strip, which
                // they leave on screen, goes on to switch.
                if (self.view.help || self.view.trash.is_some()) && id != Some(PaneId::Help) {
                    if !std::mem::take(&mut self.view.help) {
                        self.view.trash = None;
                    }
                    self.view.press = None;
                    if !matches!(id, Some(PaneId::Tab(_))) {
                        return Vec::new();
                    }
                }
                self.press(x, y);
                self.view.selection = pane.map(|p| Selection::start(&p, x, y));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(sel) = self.view.selection.as_mut().filter(|s| !s.done) {
                    sel.extend(x, y);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let (click, target) = self.tapped(x, y);
                if let Some(id) = self.double_under_popup(x, y).filter(|_| click) {
                    self.view.last_click = None;
                    self.view.selection = None;
                    self.view.action = None;
                    self.dive(&id);
                    return Vec::new();
                }
                if let Some(t) = target {
                    self.view.selection = None;
                    return self.tap(t);
                }
                // A question waiting for `y`: a click elsewhere cancels it, as any other key
                // does, and does nothing else.
                if click && self.view.confirm.is_some() {
                    self.view.confirm = None;
                    self.view.selection = None;
                    self.view.status = Some("cancelled".into());
                    return Vec::new();
                }
                let Some(mut sel) = self.view.selection.take().filter(|s| !s.done) else {
                    return Vec::new();
                };
                sel.extend(x, y);
                if sel.is_click() {
                    if let PaneId::Tab(name) = &sel.pane.id {
                        return self.switch_workspace(name);
                    }
                    // A click is about something else than the action popup's rows: it
                    // closes the popup, then does what it does.
                    self.view.action = None;
                    // A running `!` command's `■ stop`: the same one under the press as under
                    // the release (output streaming in may have moved another one there).
                    let pressed = sel.pane.link(sel.anchor.y);
                    let stop = (sel.pane.id == PaneId::Transcript)
                        .then(|| {
                            self.view
                                .panes
                                .get(&PaneId::Transcript)
                                .filter(|p| p.area == sel.pane.area)
                                .and_then(|p| p.link(y))
                                .filter(|l| pressed == Some(*l))
                                .and_then(|l| l.strip_prefix(crate::ui::SHELL_STOP))
                                .map(str::to_string)
                        })
                        .flatten();
                    if let Some(job) = stop {
                        return self.stop_shell(&job);
                    }
                    self.click(&sel.pane, x, y);
                    return Vec::new();
                }
                // The rows and the cells to leave out as last drawn, the draw `screen` comes
                // from (a transcript may have moved, a cursor moved on, since the drag began).
                sel.refresh(&self.view.panes);
                let text = sel.text(screen);
                if text.trim().is_empty() {
                    return Vec::new();
                }
                let n = text.chars().count();
                self.view.status = Some(format!(
                    "copied {n} character{}",
                    if n == 1 { "" } else { "s" }
                ));
                sel.done = true;
                self.view.selection = Some(sel);
                return vec![Action::Copy { text }];
            }
            // While the TUI has the mouse, the terminal no longer pastes on a middle click.
            MouseEventKind::Up(MouseButton::Middle) => {
                self.view.status = Some(MIDDLE_CLICK_HINT.into());
            }
            MouseEventKind::ScrollUp => self.wheel(x, y, true),
            MouseEventKind::ScrollDown => self.wheel(x, y, false),
            _ => {}
        }
        Vec::new()
    }

    /// The left button went down at `(x, y)`: on the control the last frame drew there, if
    /// any ([`ViewState::press`]).
    fn press(&mut self, x: u16, y: u16) {
        let target = self.view.panes.target_at(x, y).cloned();
        let about = target.as_ref().and_then(|t| self.press_about(t));
        self.view.press = Some((x, y, target, about));
    }

    /// What control `t` acts on, for the controls whose meaning changes under them: an action
    /// popup row, the card or request the popup is on and its item's revision; `yes` and `no`,
    /// the question waiting for `y`; the take-back, the message (its thread and ref). `None`
    /// for the rest.
    fn press_about(&self, t: &Target) -> Option<String> {
        match t {
            Target::Row(_) => {
                let item = self
                    .selected_attention()
                    .map(|i| (i.attn_id.clone(), i.revision));
                let on = self.view.action.as_ref().map(|p| p.target.clone());
                Some(format!("{on:?} {item:?}"))
            }
            Target::Yes | Target::No => Some(format!("{:?}", self.view.confirm)),
            Target::TakeBack => Some(format!("{:?}", self.unsend_check().ok())),
            _ => None,
        }
    }

    /// The left button came up at `(x, y)`: whether it was a click (a tap, where it went
    /// down: [`crate::select::is_tap`]), and the control it tapped: the one it went down on,
    /// still drawn where it came up. The frame drawn in between may have put something else
    /// there (the help closed, rows moved): that is not tapped. A popup row pressed on one
    /// item (or revision) and released on another is neither tapped nor a click.
    fn tapped(&mut self, x: u16, y: u16) -> (bool, Option<Target>) {
        let Some((px, py, target, about)) = self.view.press.take() else {
            return (false, None);
        };
        let click = crate::select::is_tap(
            ratatui::layout::Position { x: px, y: py },
            ratatui::layout::Position { x, y },
        );
        // Pressed on a control about something else than what it is about now (a popup row
        // of another card, item or revision; `yes` to another question; the take-back of
        // another message): nothing runs, and,
        // taken as a click on nothing, it would close the popup or cancel the question: they
        // stay, saying why.
        if let Some(t) = target.as_ref().filter(|_| click) {
            if about.is_some() && self.press_about(t) != about {
                self.view.status =
                    Some("it changed while you tapped: read it, then tap again".into());
                self.view.selection = None;
                return (false, None);
            }
        }
        let target = target.filter(|t| click && self.view.panes.target_at(x, y) == Some(t));
        (click, target)
    }

    /// The second click of a double click at `(x, y)` on the card or tile the first one
    /// selected, whose action popup (the first click opened it, the card being selected
    /// already) now covers the spot: the worker to dive into. Not another popup (the `/`
    /// one), nor with a question waiting for `y`.
    fn double_under_popup(&mut self, x: u16, y: u16) -> Option<String> {
        let (id, at) = self.view.last_click.clone()?;
        let (lx, ly) = self.view.last_click_at?;
        let its = self
            .action_popup()
            .is_some_and(|p| p.target == ActionTarget::Card(id.clone()));
        let covered = its
            && self.view.confirm.is_none()
            && self.view.panes.at(x, y).map(|p| &p.id) == Some(&PaneId::Popup);
        let near = crate::select::is_tap(
            ratatui::layout::Position { x: lx, y: ly },
            ratatui::layout::Position { x, y },
        );
        let worker = self.registry.get(&id).is_some_and(|t| !t.is_orchestrator());
        (covered && near && worker && at.elapsed() < DOUBLE_CLICK).then_some(id)
    }

    /// A tap on control `target` ([`Target`]): what its key or command does, but the action
    /// popup's rows ([`AppState::tap_row`]) and `yes` to a question waiting for `y`, which a
    /// tap runs only once what it answers has been read. With a question waiting, any other
    /// tap cancels it, as any other key does.
    fn tap(&mut self, target: Target) -> Vec<Action> {
        if self.view.confirm.is_some() && !matches!(target, Target::Yes | Target::No) {
            self.view.confirm = None;
            self.view.status = Some("cancelled".into());
            return Vec::new();
        }
        // Anything but its own rows and `✕` is about something else: the popup closes first.
        if !matches!(target, Target::Row(_) | Target::Close) {
            self.view.action = None;
        }
        match target {
            Target::Screen(pane) => {
                if self.compact() && self.view.screen != Screen::Decisions {
                    self.show_pane(pane);
                }
            }
            Target::Back => {
                if self.view.dive.is_some() {
                    self.leave_dive();
                    self.view.status = None;
                }
            }
            Target::Stack => {
                if self.view.screen == Screen::Grid {
                    self.toggle_grid();
                }
            }
            Target::Follow => {
                self.view.request = None;
                self.view.shown = None;
                self.follow();
            }
            Target::Close => return self.close_top(),
            // A queued message: a tap selects it, a tap on the selected one opens its popup.
            Target::Queued(r) => {
                if self.view.queued == Some(r) {
                    if let Some(t) = self.queued_target() {
                        self.open_action(t);
                    }
                } else if self.shown_queue().is_some() {
                    self.select_queued(r);
                }
            }
            Target::Release => {
                if self.view.request.take().is_some() {
                    self.view.shown = None;
                    self.view.detail = None;
                    self.follow();
                } else {
                    self.view.detail = None;
                }
            }
            Target::TakeBack => return self.tap_unsend(Instant::now()),
            Target::Row(kind) => return self.tap_row(kind),
            Target::Suggestion(value) => {
                if let Some(s) = self.popup() {
                    if let Some(i) = s.items.iter().position(|item| item.value == value) {
                        self.popup_complete(&s, i);
                    }
                }
            }
            Target::Runtime(i) => {
                if let Some(p) = self.view.picker.as_mut().filter(|p| i < p.runtimes.len()) {
                    if p.selected != i {
                        p.selected = i;
                        return Vec::new();
                    }
                    return self.picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                }
            }
            Target::Decision(id) => {
                let at = self.decision_rows().iter().position(|r| r.id == id);
                if let Some(i) = at.filter(|_| self.view.screen == Screen::Decisions) {
                    if self.decision_selected() == i {
                        self.open_decision();
                    } else {
                        self.view.decision_sel = i;
                    }
                }
            }
            Target::SiteFilter | Target::ProviderFilter => {
                if self.view.screen == Screen::Decisions {
                    let code = if target == Target::SiteFilter {
                        'f'
                    } else {
                        'v'
                    };
                    let key = KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE);
                    return self.decisions_key(key).unwrap_or_default();
                }
            }
            Target::Yes => return self.tap_yes(),
            Target::No => {
                if self.view.confirm.take().is_some() {
                    self.view.status = Some("cancelled".into());
                }
            }
            Target::Notice => {
                self.view.status = None;
                self.view.status_until = None;
            }
            Target::Sidebar => self.set_sidebar(true),
            Target::FocusOff => self.set_focus_mode(false),
            Target::Parked => self.show_parked(true),
        }
        Vec::new()
    }

    /// The `✕` of what is open on top (the one control of its kind the last frame drew, the
    /// rest covered): closes it as Esc does.
    fn close_top(&mut self) -> Vec<Action> {
        if self.picker_open() {
            return self.picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        let closed = std::mem::take(&mut self.view.help)
            || self.view.trash.take().is_some()
            || self.view.action.take().is_some();
        if closed {
            return Vec::new();
        }
        if self.popup().is_some() {
            self.view.popup_closed = Some(self.view.input.as_str().to_string());
        } else if self.view.screen == Screen::Decisions {
            self.toggle_decisions();
        }
        Vec::new()
    }

    /// A tap on `yes` of the question waiting for `y`: it runs as `y` does, once the question
    /// has been on screen for [`CARD_SETTLE`] ([`ViewState::confirm_seen`]); before that it
    /// says so, and the question stays.
    fn tap_yes(&mut self) -> Vec<Action> {
        let Some(c) = &self.view.confirm else {
            return Vec::new();
        };
        let read = self
            .view
            .confirm_seen
            .as_ref()
            .is_some_and(|(prompt, since)| *prompt == c.prompt && since.elapsed() >= CARD_SETTLE);
        if !read {
            return Vec::new();
        }
        let Some(c) = self.view.confirm.take() else {
            return Vec::new();
        };
        self.view.status = Some(c.pending);
        vec![c.action]
    }

    /// Whether the input box offers to take back the last message ([`Target::TakeBack`]):
    /// nothing is typed, the main pane shows the message's thread, and it can still be taken
    /// back.
    pub fn take_back_offered(&self) -> bool {
        let here = self.main_thread();
        self.view.input.is_empty()
            && self.sent.as_ref().is_some_and(|s| {
                s.thread == here
                    || (s.armed.is_some() && s.armed_in.as_ref() == Some(&self.armed_view()))
            })
            && self.unsend_check().is_ok()
    }

    /// A tap on the input box's take-back control at `now`: the first arms the second, which
    /// takes the message back within [`UNSEND_WINDOW`], as Esc Esc does (the hint in the
    /// box's title says so meanwhile).
    fn tap_unsend(&mut self, now: Instant) -> Vec<Action> {
        self.expire_unsend(now);
        if !self.take_back_offered() {
            return Vec::new();
        }
        let view = self.armed_view();
        if self
            .sent
            .as_ref()
            .is_some_and(|s| s.armed.is_some() && s.armed_in.as_ref() == Some(&view))
        {
            return self.unsend_armed();
        }
        if let Some(s) = self.sent.as_mut() {
            s.armed = Some(now);
            s.armed_in = Some(view);
        }
        Vec::new()
    }

    /// A tap on the action popup's row `kind`, as the last frame drew it: one the filter
    /// leaves out does nothing, one not highlighted is highlighted, and the highlighted one
    /// runs as Enter runs it, under the same guards. An answer, Dismiss or Stop runs only once
    /// a tap highlighted it on the item as it is now ([`ActionPopup::armed`]), and that tap
    /// counted only once the item had been on screen for [`CARD_SETTLE`]: a stray tap, a tap
    /// meant for what was there before, or the second tap of a double tap meant for a row
    /// under the popup, answers nothing.
    fn tap_row(&mut self, kind: ActionKind) -> Vec<Action> {
        let Some(popup) = self.action_popup().cloned() else {
            return Vec::new();
        };
        let rows = self.action_rows(&popup.target);
        let matches = self.action_matches(&popup, &rows);
        let Some(row) = rows.iter().position(|r| r.kind == kind) else {
            return Vec::new();
        };
        if !matches.contains(&row) {
            self.view.status = Some(format!(
                "the filter {} leaves that row out (Backspace)",
                popup.filter
            ));
            return Vec::new();
        }
        let drawn = self
            .view
            .panes
            .popup_rows
            .as_ref()
            .is_some_and(|(t, kinds)| *t == popup.target && kinds.contains(&kind));
        if !drawn {
            return Vec::new();
        }
        let item = self
            .selected_attention()
            .map(|i| (i.attn_id.clone(), i.revision));
        let guarded = matches!(
            kind,
            ActionKind::Answer(_)
                | ActionKind::Dismiss
                | ActionKind::Stop
                | ActionKind::Delete
                | ActionKind::SendNow
        );
        // On screen in full (the popup, or the main pane, drew it whole: [`ViewState::shown`])
        // and read: unchanged for CARD_SETTLE.
        let shown = match &item {
            None => Some(true),
            Some((id, rev)) => self
                .view
                .shown
                .as_ref()
                .filter(|s| s.attn_id == *id && s.revision == *rev)
                .map(|s| s.since.elapsed() >= CARD_SETTLE),
        };
        let read = shown == Some(true);
        // Armed on this item, and the item still on screen in full: it runs.
        let armed = popup.armed.as_ref() == Some(&(kind, item.clone()));
        if popup.sel == Some(kind) && (!guarded || (armed && read)) {
            return self.run_action(kind);
        }
        if let Some(p) = self.view.action.as_mut() {
            p.sel = Some(kind);
            p.armed = (guarded && read).then_some((kind, item));
        }
        if guarded {
            let label = rows[row]
                .label
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            self.view.status = Some(match shown {
                Some(true) => format!("tap {label} again to run it"),
                Some(false) => "just shown: read it, then tap again".to_string(),
                None => NOT_ON_SCREEN.to_string(),
            });
        }
        Vec::new()
    }

    /// A click without a drag at `(x, y)` of `pane`.
    ///
    /// On a card, tile or compact row it selects that thread; on the one selected it opens its
    /// action popup. A second click on a card or tile soon after the first ([`DOUBLE_CLICK`])
    /// dives into its thread instead; a compact row has no timing: a tap on the selected row
    /// opens its popup (Talk to it dives from there), so a double tap does too. On a request in
    /// the conversation it selects the request, and on the selected one it opens its popup.
    fn click(&mut self, pane: &crate::select::Pane, x: u16, y: u16) {
        match &pane.id {
            // The detail view of the selection's item occupies this pane (a request, or an
            // item its popup had no room for, in a dive too; its card listed or not: the
            // sidebar may be hidden). Clicking its text keeps that exact item selected,
            // including on a second click, and opens its popup.
            PaneId::Thread(id)
                if self
                    .view
                    .panes
                    .card
                    .as_ref()
                    .is_some_and(|c| c.thread == *id && c.area == pane.area)
                    && match self.selection_target() {
                        Some(ActionTarget::Card(card)) => card == *id,
                        Some(ActionTarget::Request(_)) => {
                            self.selected_request().is_some_and(|i| i.thread == *id)
                        }
                        Some(ActionTarget::Queued(_)) | None => false,
                    } =>
            {
                if let Some(t) = self.selection_target() {
                    self.open_action(t);
                }
            }
            PaneId::Thread(id) if self.nav_order().contains(id) => {
                let selected = self.focused() == Some(id.as_str()) && !self.main_focused();
                if self.compact_listed() {
                    self.view.last_click = None;
                    if selected {
                        if let Some(t) = self.selection_target() {
                            self.open_action(t);
                        }
                    } else {
                        self.set_focus(Some(id.clone()));
                        self.view.main_focus = false;
                    }
                    return;
                }
                // A second click on the same worker's card soon after the first dives in.
                let double = self
                    .view
                    .last_click
                    .take()
                    .is_some_and(|(last, at)| last == *id && at.elapsed() < DOUBLE_CLICK);
                // The card clicked is on screen: the cards stay where they are drawn (the
                // slice the last frame showed, which has it, [`crate::ui`]'s `list_start`).
                if !selected {
                    self.set_focus(Some(id.clone()));
                }
                self.view.main_focus = false;
                if double && self.focused_is_worker() {
                    self.dive(id);
                    return;
                }
                self.view.last_click = Some((id.clone(), Instant::now()));
                self.view.last_click_at = Some((x, y));
                if selected {
                    if let Some(t) = self.selection_target() {
                        self.open_action(t);
                    }
                }
            }
            PaneId::Transcript => {
                // A collapsed group's marker opens that group.
                let key = self
                    .view
                    .panes
                    .get(&PaneId::Transcript)
                    .filter(|p| p.area == pane.area)
                    .and_then(|p| p.link(y))
                    .map(str::to_string);
                if let Some(key) = key {
                    if let Some(id) = key.strip_prefix("request:") {
                        self.view.block = None;
                        // The request selected already: a click on it opens its popup.
                        if self.selected_request().is_some_and(|i| i.attn_id == id) {
                            if let Some(t) = self.selection_target() {
                                self.open_action(t);
                            }
                        } else {
                            self.select_request(id);
                        }
                    } else if let Some(block) = key.strip_prefix("toggle:block:") {
                        self.toggle_block(Some(format!("block:{block}")));
                    } else if key.starts_with("block:") {
                        self.view.block = Some(key);
                        self.view.request = None;
                        self.view.queued = None;
                    } else {
                        self.view.expanded.insert(key);
                    }
                }
                // A click in the conversation selects it, as Shift-← does.
                self.view.main_focus = true;
            }
            PaneId::Input => {
                // The box as drawn: its rows wrap at its width and scroll in its height.
                let (col, row) = (
                    usize::from(x.saturating_sub(pane.area.x)),
                    usize::from(y.saturating_sub(pane.area.y)),
                );
                let (cols, rows) = (usize::from(pane.area.width), usize::from(pane.area.height));
                let line = &mut self.view.input;
                line.click(col, row, cols, rows);
                line.keep_visible(cols, rows);
            }
            _ => {}
        }
    }

    /// The wheel over `(x, y)`: the transcript scrolls, the decision table moves its
    /// selection, the cards (a column, a row, the grid's tiles, a compact list) scroll a card
    /// (a row of tiles), leaving the selection where it is; the action popup and the `/` or
    /// `@` popup move their highlight, as ↑ and ↓ do; an input box taller than it shows moves
    /// its cursor a row.
    fn wheel(&mut self, x: u16, y: u16, up: bool) {
        self.view.selection = None;
        // An overlay scrolls wherever the pointer is: what is under it is not on screen.
        let step = WHEEL_STEP as u16;
        let wheel = |s: &mut u16, max: u16| {
            // From where it shows: a resize may have left the stored scroll past the end.
            let at = (*s).min(max);
            *s = if up {
                at.saturating_sub(step)
            } else {
                at.saturating_add(step).min(max)
            };
        };
        if self.view.help {
            wheel(&mut self.view.help_scroll, self.view.panes.help_max);
            return;
        }
        if self.view.trash.is_some() {
            wheel(&mut self.view.trash_scroll, self.view.panes.trash_max);
            return;
        }
        let arrow = KeyEvent::new(
            if up { KeyCode::Up } else { KeyCode::Down },
            KeyModifiers::NONE,
        );
        match self.view.panes.at(x, y).map(|p| (p.id.clone(), p.area)) {
            Some((PaneId::Popup, _)) => {
                if self.action_popup().is_some() {
                    self.action_key(arrow);
                } else {
                    self.popup_key(arrow);
                }
            }
            Some((PaneId::Transcript, _)) => self.scroll_transcript(!up, WHEEL_STEP),
            Some((PaneId::Decisions, _)) => {
                let step = WHEEL_STEP as isize;
                self.move_decision(if up { -step } else { step });
            }
            Some((PaneId::Input, area)) => {
                let (cols, rows) = (usize::from(area.width), usize::from(area.height));
                let line = &mut self.view.input;
                if line.rows(cols).len() > rows {
                    if up {
                        line.up(cols);
                    } else {
                        line.down(cols);
                    }
                    line.keep_visible(cols, rows);
                }
            }
            Some((PaneId::Thread(id), _))
                if self
                    .view
                    .panes
                    .card
                    .as_ref()
                    .is_some_and(|c| c.thread == id && c.area.contains((x, y).into())) =>
            {
                self.scroll_card(!up, WHEEL_STEP);
            }
            None | Some((PaneId::Thread(_) | PaneId::Text, _)) => self.scroll_cards(x, y, up),
            _ => {}
        }
    }

    /// The wheel over `(x, y)` scrolls the cards when they are there, as last drawn: a card
    /// (a row of tiles) up or down, within the list.
    fn scroll_cards(&mut self, x: u16, y: u16, up: bool) {
        let Some(list) = self
            .view
            .panes
            .list
            .filter(|l| l.area.contains((x, y).into()))
        else {
            return;
        };
        let start = if up {
            list.start.saturating_sub(1)
        } else {
            // Past `max` (its first card kept first): no further.
            (list.start + 1).min(list.max.max(list.start))
        };
        let first = match list.layout {
            crate::select::ListLayout::Grid(cols) => self.grid_order().get(start * cols).cloned(),
            crate::select::ListLayout::Compact(_) => self.nav_order().get(start).cloned(),
            _ => self.order().get(start).cloned(),
        };
        self.view.card_scroll = Some(CardScroll {
            start,
            first,
            focus: self.view.focus.clone(),
            layout: list.layout,
        });
    }

    /// Text pasted into the terminal (bracketed paste): inserted at the cursor of the input
    /// line, its line breaks kept; an open action popup closes first. Never a key: a pasted `1`
    /// answers nothing, a pasted line break sends nothing. The decisions overlay has no input
    /// box: a paste there does nothing.
    pub fn handle_paste(&mut self, text: &str) {
        if self.picker_open() {
            return;
        }
        self.view.selection = None;
        // Like a key, a paste closes the help or cancels a confirmation, and does nothing else.
        if std::mem::take(&mut self.view.help) {
            return;
        }
        if self.view.confirm.take().is_some() {
            self.view.status = Some("cancelled".into());
            return;
        }
        if self.view.screen == Screen::Decisions {
            return;
        }
        self.view.action = None;
        let (cols, rows) = (self.input_cols(), self.input_rows());
        self.view.input.insert(text);
        self.view.input.keep_visible(cols, rows);
    }

    /// Answers the selected card's top attention item (or the request selected in the
    /// conversation) with option `n` (1-based): from the action popup only, its digits or its
    /// rows.
    fn answer(&mut self, n: usize) -> Vec<Action> {
        let Some(item) = self.selected_attention() else {
            self.view.status = Some("nothing to answer on this thread".into());
            return Vec::new();
        };
        // Only the card the last frame drew in full, with its options, once the user has had
        // time to read it: one drawn just now replaced what the key was meant for, and one not
        // drawn (hidden in this view, scrolled away, cut short) was never read.
        let Some(s) = &self.view.shown else {
            self.view.status = Some(NOT_ON_SCREEN.into());
            return Vec::new();
        };
        let same = s.attn_id == item.attn_id && s.revision == item.revision;
        if !same || s.since.elapsed() < CARD_SETTLE {
            self.view.status = Some("the question just changed; read it and answer again".into());
            return Vec::new();
        }
        match item.options.get(n - 1) {
            Some(decision) => {
                let action = Action::Respond {
                    attn_id: item.attn_id.clone(),
                    revision: item.revision,
                    decision: decision.clone(),
                };
                self.view.status = None;
                vec![action]
            }
            None => {
                self.view.status = Some(format!("no option {n}"));
                Vec::new()
            }
        }
    }
}

/// Applies an editing key to `line` (a box `cols` cells wide that grows to `rows` rows):
/// cursor motion (Left/Right; Up/Down a row of the wrapped text; Home/End the start and end
/// of the cursor's row, Ctrl-A/E of the whole text; by word with Ctrl or Alt and Left/Right,
/// Alt-B/F), deletion (Backspace, Delete, Ctrl-W the word before, Alt-Backspace the same,
/// Ctrl-K to the end of the line, Ctrl-U the whole text) and typed characters (inserted at
/// the cursor).
/// Returns whether it was one. The `/` or `@` popup takes Up and Down first while it is open;
/// Up and Down with Ctrl or Alt scroll the main pane, with Shift they move the selection
/// ([`AppState::view_key`], [`AppState::selection_key`]): they are no rows of the line.
/// The message a line typed in the input box sends ([`AppState::input_key`]): `\!...` sends
/// `!...` (one backslash of those before a `!` goes), `//...` sends `/...`; anything else as
/// it is. A line that starts with `!` or a single `/` sends nothing: it is a command.
pub fn message_text(line: &str) -> String {
    if line.starts_with('\\') && line.trim_start_matches('\\').starts_with('!') {
        return line[1..].to_string();
    }
    match line.strip_prefix('/') {
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        _ => line.to_string(),
    }
}

/// The line that sends `message` as it is ([`message_text`]'s inverse): what goes back in the
/// input box or the history for a message taken back or a suggestion taken, so Enter sends it
/// again as a message, never as a `!` or `/` command. Leading blanks go, as Enter trims them.
pub fn literal_line(message: &str) -> String {
    // As the box will hold it: control characters a message may start with go first, so they
    // never hide a `!` or `/` from the escape.
    let clean = crate::edit::clean_text(message);
    let t = clean.trim_start();
    if t.trim_start_matches('\\').starts_with('!') {
        format!("\\{t}")
    } else if t.starts_with('/') {
        format!("/{t}")
    } else {
        t.to_string()
    }
}

fn edit_key(line: &mut LineInput, key: KeyEvent, cols: usize, rows: usize) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Left if ctrl || alt => line.word_left(),
        KeyCode::Right if ctrl || alt => line.word_right(),
        KeyCode::Left => line.left(),
        KeyCode::Right => line.right(),
        KeyCode::Up if !ctrl && !alt => line.up(cols),
        KeyCode::Down if !ctrl && !alt => line.down(cols),
        KeyCode::Home => line.row_home(cols),
        KeyCode::End => line.row_end(cols),
        KeyCode::Backspace if alt => line.delete_word_before(),
        KeyCode::Backspace => line.backspace(),
        KeyCode::Delete => line.delete(),
        KeyCode::Char('a') if ctrl => line.home(),
        KeyCode::Char('e') if ctrl => line.end(),
        KeyCode::Char('b') if alt => line.word_left(),
        KeyCode::Char('f') if alt => line.word_right(),
        // Some terminals send Ctrl-H for Backspace.
        KeyCode::Char('h') if ctrl => line.backspace(),
        KeyCode::Char('w') if ctrl => line.delete_word_before(),
        KeyCode::Char('k') if ctrl => line.kill_to_end(),
        KeyCode::Char('u') if ctrl => line.clear(),
        KeyCode::Char(c) if !ctrl && !alt => {
            let mut buf = [0u8; 4];
            line.insert(c.encode_utf8(&mut buf));
        }
        _ => return false,
    }
    line.keep_visible(cols, rows);
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use twr_core::event::{AgentText, AgentToolCall, AgentToolResult, AttentionResolved};
    use twr_core::testkit::ThreadScript;
    use AttentionReason::*;

    pub(crate) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// `code` with Shift: the keys that move the selection.
    pub(crate) fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    /// Opens the action popup on the selection (Enter on an empty line) and draws it, settled:
    /// what it shows has been on screen long enough to be read ([`settled`]).
    pub(crate) fn popup_open(st: &mut AppState) {
        assert!(st.view.input.is_empty(), "Enter on an empty line");
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.action.is_some(), "the popup opened");
        settled(st);
    }

    /// Runs the action popup's row `kind`, the popup opened on the selection and settled
    /// ([`popup_open`]): ↓ to it, then Enter.
    pub(crate) fn run_row(st: &mut AppState, kind: ActionKind) -> Vec<Action> {
        popup_open(st);
        let target = st.view.action.as_ref().unwrap().target.clone();
        let rows = st.action_rows(&target);
        assert!(
            rows.iter().any(|r| r.kind == kind),
            "no {kind:?} row: {rows:?}"
        );
        while st.view.action.as_ref().unwrap().sel != Some(kind) {
            st.handle_key(key(KeyCode::Down));
        }
        st.handle_key(key(KeyCode::Enter))
    }

    /// Shift-`code` (↑ or ↓) with the main pane selected (Shift-← first when it is not): the
    /// previous or next long block or request of the conversation.
    pub(crate) fn step(st: &mut AppState, code: KeyCode) {
        if !st.main_focused() {
            st.handle_key(shift(KeyCode::Left));
        }
        st.handle_key(shift(code));
    }

    /// Answers with option `n` from the action popup, opened on the selection and settled.
    pub(crate) fn answer_with(st: &mut AppState, n: u32) -> Vec<Action> {
        popup_open(st);
        st.handle_key(key(KeyCode::Char(char::from_digit(n, 10).unwrap())))
    }

    pub(crate) fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    pub(crate) fn control(state: &mut AppState, events: &[Event]) {
        for ev in events {
            let g = state.last_gseq + 1;
            state.apply_event(&StreamItem {
                stream: StreamKind::Control,
                gseq: Some(g),
                event: ev.clone(),
            });
        }
    }

    pub(crate) fn thread_item(ev: &Event) -> StreamItem {
        StreamItem {
            stream: StreamKind::Thread,
            gseq: None,
            event: ev.clone(),
        }
    }

    /// Transcript-only fixtures leave worker requests resolved, so row/cursor assertions
    /// exercise the conversation independently of live inline cards.
    pub(crate) fn without_worker_requests(mut st: AppState) -> AppState {
        let ids = st
            .inline_requests()
            .iter()
            .map(|i| (i.thread.clone(), i.attn_id.clone()))
            .collect::<Vec<_>>();
        for (thread, id) in ids {
            let mut script = ThreadScript::new(thread);
            script.at(90);
            control(&mut st, &[script.resolve(&id)]);
        }
        st
    }

    /// `alpha`: running with a review item (oldest); `beta`: permission; `gamma`: running, no
    /// attention. Stack order: beta, alpha, gamma.
    fn typed(st: &mut AppState, text: &str) {
        for c in text.chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
    }

    /// Up on the input line's first row recalls the lines sent before, newest first; Down
    /// goes forward again, and past the newest back to what was being typed. A line sent
    /// twice in a row is kept once; commands are kept too.
    #[test]
    fn up_and_down_browse_the_lines_sent_before() {
        let mut st = with_orch();
        st.view.width = 80;
        // No history: Up is an ordinary cursor move.
        typed(&mut st, "x");
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.view.input.as_str(), "x");
        assert_eq!(st.view.input.cursor(), 0);
        st.view.input.clear();
        for line in ["first", "second", "second", "/help"] {
            typed(&mut st, line);
            st.handle_key(key(KeyCode::Enter));
            st.view.help = false;
        }
        assert_eq!(st.view.history, ["first", "second", "/help"]);
        assert_eq!(st.view.history_new, ["first", "second", "/help"]);
        typed(&mut st, "draft");
        let up = |st: &mut AppState| {
            st.handle_key(key(KeyCode::Up));
            st.view.input.as_str().to_string()
        };
        let down = |st: &mut AppState| {
            st.handle_key(key(KeyCode::Down));
            st.view.input.as_str().to_string()
        };
        assert_eq!(up(&mut st), "/help");
        assert!(st.popup().is_none(), "a recalled command opens no popup");
        assert_eq!(up(&mut st), "second");
        assert_eq!(up(&mut st), "first");
        assert_eq!(up(&mut st), "first", "the oldest stays");
        assert_eq!(st.view.input.cursor(), "first".len());
        assert_eq!(down(&mut st), "second");
        assert_eq!(down(&mut st), "/help");
        assert_eq!(down(&mut st), "draft");
        assert_eq!(st.browsing(), None);
        assert_eq!(down(&mut st), "draft");
        // A recalled line is sent like a typed one, and browsing starts over.
        up(&mut st);
        up(&mut st);
        let sent = st.handle_key(key(KeyCode::Enter));
        assert!(matches!(&sent[..], [Action::Say { text, .. }] if text == "second"));
        assert_eq!(st.view.history.last().map(String::as_str), Some("second"));
        assert_eq!(up(&mut st), "second");
        // Editing it ends browsing: it is what is being typed now.
        typed(&mut st, "!");
        assert_eq!(st.browsing(), None);
        assert_eq!(up(&mut st), "second");
        assert_eq!(down(&mut st), "second!");
        // So does a paste: Down then keeps the pasted line.
        assert_eq!(up(&mut st), "second");
        st.handle_paste(" and more");
        assert_eq!(st.browsing(), None);
        assert_eq!(down(&mut st), "second and more");
        // On a line wrapped over rows, Up moves between them first.
        st.view.width = 22;
        st.view
            .input
            .set("one two three four five six seven eight nine");
        st.view.history_pos = None;
        st.handle_key(key(KeyCode::Up));
        assert_eq!(
            st.view.input.as_str(),
            "one two three four five six seven eight nine"
        );
        // An earlier session's lines come before this one's.
        st.load_history(vec!["old".into()]);
        assert_eq!(st.view.history.first().map(String::as_str), Some("old"));
    }

    /// Tab selects a card while typing goes on: with none selected the first in attention
    /// order, then the next; Shift-Tab the previous. What was typed stays, and typing goes on
    /// after it. Ctrl-L asks for a full repaint, whatever is open.
    #[test]
    fn tab_selects_a_card_while_typing_goes_on_and_ctrl_l_repaints() {
        let mut st = three_threads();
        assert_eq!(st.focused(), None);
        typed(&mut st, "hi");
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-beta"));
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-alpha"));
        st.handle_key(shift(KeyCode::BackTab));
        assert_eq!(st.focused(), Some("t-beta"));
        typed(&mut st, "!");
        assert_eq!(st.view.input.as_str(), "hi!");
        // Shift-Tab with none selected: the last.
        st.set_focus(None);
        st.handle_key(shift(KeyCode::BackTab));
        assert_eq!(st.focused(), Some("t-gamma"));
        st.view.help = true;
        st.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(st.view.repaint);
        assert!(st.view.help, "the help stays open");
    }

    /// Every read of the orchestrator, served or refused, is a hub line of its conversation.
    #[test]
    fn project_reads_show_as_hub_lines() {
        let mut t = Transcript::new("t-orch");
        let mut o = ThreadScript::new("t-orch");
        t.apply(
            &o.event(EventBody::ProjectRead(twr_core::event::ProjectRead {
                tool: "read".into(),
                project: "api".into(),
                path: Some("README.md".into()),
                commit: Some("1a2b3c4d5e6f7a8b".into()),
                bytes: 4300,
                ..Default::default()
            })),
        );
        t.apply(
            &o.event(EventBody::ProjectRead(twr_core::event::ProjectRead {
                tool: "read".into(),
                project: "api".into(),
                path: Some(".env".into()),
                refused: Some(".env is an environment file".into()),
                ..Default::default()
            })),
        );
        let texts: Vec<&str> = t
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::HubUpdate { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "read api:README.md (4 KB), at 1a2b3c4d5e6f",
                "refused to read api:.env: .env is an environment file",
            ]
        );
    }

    /// A request's end goes onto the hub update that raised it: one entry, its outcome
    /// filled in. An end whose raise is not in the transcript stays an update of its own.
    #[test]
    fn a_requests_end_merges_into_its_update() {
        let mut t = Transcript::new("t-orch");
        let mut o = ThreadScript::new("t-orch");
        let update = |o: &mut ThreadScript, text: &str, id: &str, resolved: bool| {
            let mut twr = serde_json::json!({"attn_id": id});
            if resolved {
                twr["resolved"] = true.into();
            }
            o.event(EventBody::ConversationUpdate(
                twr_core::event::ConversationUpdate {
                    text: text.into(),
                    about_thread: Some("t-api".into()),
                    ext: serde_json::json!({ "twr": twr }).as_object().cloned(),
                    ..Default::default()
                },
            ))
        };
        t.apply(&update(
            &mut o,
            "api needs permission: Allow shell: ls -a",
            "a1",
            false,
        ));
        // Raised again: one update of it.
        t.apply(&update(
            &mut o,
            "api needs permission: Allow shell: ls",
            "a1",
            false,
        ));
        t.apply(&update(
            &mut o,
            "api request a1 answered: deny.",
            "a1",
            true,
        ));
        t.apply(&update(&mut o, "api request a7 closed.", "a7", true));
        assert_eq!(t.entries.len(), 2, "{:?}", t.entries);
        assert!(matches!(
            &t.entries[0],
            Entry::HubUpdate { outcome: Some(o), about: Some(a), .. }
                if o == "answered: deny" && a == "t-api"
        ));
        assert!(matches!(
            &t.entries[1],
            Entry::HubUpdate { outcome: None, .. }
        ));
        // An end whose raise is not here stays, even when its id is raised again; the new
        // raise's end goes onto the new raise.
        t.apply(&update(
            &mut o,
            "api needs permission: Allow shell: pwd",
            "a7",
            false,
        ));
        assert_eq!(t.entries.len(), 3, "{:?}", t.entries);
        t.apply(&update(
            &mut o,
            "api request a7 answered: allow.",
            "a7",
            true,
        ));
        assert_eq!(t.entries.len(), 3, "{:?}", t.entries);
        assert!(matches!(
            &t.entries[1],
            Entry::HubUpdate {
                resolution: true,
                outcome: None,
                ..
            }
        ));
        assert!(matches!(
            &t.entries[2],
            Entry::HubUpdate {
                outcome: Some(_),
                ..
            }
        ));
        assert_eq!(request_outcome("x request y closed."), "closed");
        assert_eq!(
            request_outcome("x request y answered: allow."),
            "answered: allow"
        );
        assert_eq!(
            request_outcome("x request y answered: closed."),
            "answered: closed"
        );
        assert_eq!(
            request_outcome("x request y answered: I answered: deny..."),
            "answered: I answered: deny.."
        );
    }

    /// The rows drawn of `st` at `w` x `h`, as the IO loop draws it.
    fn rows(st: &mut AppState, w: u16, h: u16) -> Vec<String> {
        let buf = draw(st, w, h);
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// The compact layout (a phone, below `[tui] compact_width` columns): one screen at a time
    /// under a tab bar naming them. "Needs you" lists each item, most urgent first, and ↑↓ and
    /// Enter open one's action popup; "Threads" shows each thread's card, the selected one kept
    /// in view; the conversation has no cards, and Enter on its empty line opens the oldest
    /// item waiting.
    #[test]
    fn the_compact_layout_shows_one_screen_at_a_time() {
        let mut st = three_threads();
        st.view.compact_width = 60;
        let lines = rows(&mut st, 50, 24);
        assert!(st.compact());
        let names: Vec<&str> = lines[1].split_whitespace().collect();
        assert_eq!(
            names,
            ["Chat", "Needs", "you", "2", "Threads"],
            "{lines:#?}"
        );
        // Needs you.
        type_line(&mut st, "/n");
        let lines = rows(&mut st, 50, 24);
        let text = lines.join("\n");
        let perm = lines
            .iter()
            .position(|l| l.contains("beta · permission · Bash: rm -rf"));
        let review = lines
            .iter()
            .position(|l| l.contains("alpha · review · turn gave up"));
        assert!(perm.is_some() && review.is_some(), "{text}");
        assert_eq!(st.focused(), Some("t-beta"));
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.focused(), Some("t-alpha"));
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.focused(), Some("t-beta"));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.action_popup().is_some());
        st.handle_key(key(KeyCode::Esc));
        assert!(st.action_popup().is_none());
        // Threads: the cards, as many as fit; ↓ moves on, and the cards follow it.
        type_line(&mut st, "/t");
        let text = rows(&mut st, 50, 24).join("\n");
        assert!(text.contains("┏ proj · beta "), "{text}");
        assert!(text.contains("PERMISSION  Bash: rm -rf target"), "{text}");
        assert!(!text.contains("gamma"), "{text}");
        st.handle_key(key(KeyCode::Down));
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.focused(), Some("t-gamma"));
        let text = rows(&mut st, 50, 24).join("\n");
        assert!(text.contains("┏ proj · gamma "), "{text}");
        assert!(text.contains("↑ 2 more"), "{text}");
        // The conversation: Enter on its empty line opens the oldest item waiting.
        type_line(&mut st, "/c");
        rows(&mut st, 50, 24);
        assert_eq!(st.view.compact_pane, CompactPane::Chat);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.compact_pane, CompactPane::Needs);
        assert!(st.action_popup().is_some());
    }

    /// On the compact lists, what the conversation had selected goes; a dive (Talk to it, `/open`)
    /// shows on the conversation screen, and the lists keep their selection during one; Enter in
    /// the conversation opens the item raised first, whichever card it is on.
    #[test]
    fn compact_screens_hand_over_to_each_other() {
        let mut st = three_threads();
        st.view.compact_width = 60;
        rows(&mut st, 50, 24);
        // The conversation selected, then the list: its selection is a card.
        st.view.main_focus = true;
        type_line(&mut st, "/n");
        assert!(!st.view.main_focus);
        st.handle_key(key(KeyCode::Enter));
        assert!(st.action_popup().is_some());
        st.handle_key(key(KeyCode::Esc));
        // A dive goes to the conversation screen.
        type_line(&mut st, "/open beta");
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        assert_eq!(st.view.compact_pane, CompactPane::Chat);
        // Back on the list during the dive, its selection holds through a draw.
        type_line(&mut st, "/t");
        st.handle_key(key(KeyCode::Down));
        let picked = st.focused().map(str::to_string);
        rows(&mut st, 50, 24);
        assert_eq!(st.focused().map(str::to_string), picked);
        // Enter in the conversation: the item raised first (alpha's review, before beta's
        // permission).
        type_line(&mut st, "/back");
        type_line(&mut st, "/c");
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.focused(), Some("t-alpha"));
    }

    /// The compact layout leaves the grid; its lists' keys move through their
    /// rows only, and its conversation's step through the conversation.
    #[test]
    fn compact_keys_act_on_what_it_draws() {
        let mut st = three_threads();
        st.view.compact_width = 60;
        rows(&mut st, 120, 30);
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        // A narrower screen leaves the grid.
        rows(&mut st, 50, 24);
        assert_eq!(st.view.screen, Screen::Stack);
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Stack);
        assert!(st.view.status.as_deref().unwrap_or("").contains("no grid"));
        // So does `/compact on`.
        rows(&mut st, 120, 30);
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        type_line(&mut st, "/compact on");
        assert_eq!(st.view.screen, Screen::Stack);
        rows(&mut st, 50, 24);
        // On a list, Shift-← selects no hidden main pane; Shift-↓ moves through the rows.
        type_line(&mut st, "/n");
        let order = st.needs_order();
        assert_eq!(st.focused(), Some(order[0].as_str()));
        st.handle_key(shift(KeyCode::Left));
        assert!(!st.view.main_focus);
        st.handle_key(shift(KeyCode::Down));
        assert_eq!(st.focused(), Some(order[1].as_str()));
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            st.action_popup().map(|p| p.target.clone()),
            Some(ActionTarget::Card(order[1].clone()))
        );
        st.handle_key(key(KeyCode::Esc));
        // The conversation: Tab selects no hidden card.
        type_line(&mut st, "/c");
        let before = st.focused().map(str::to_string);
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused().map(str::to_string), before);
        assert!(st.view.status.as_deref().unwrap_or("").contains("/n"));
    }

    /// A long item opened from "Needs you" during a dive into another thread
    /// keeps its card selected for its detail view.
    #[test]
    fn a_long_item_from_needs_keeps_its_card_during_a_dive() {
        let mut st = three_threads();
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        let mut rv = d.raise("rv9", Review);
        if let EventBody::AttentionRaised(p) = &mut rv.body {
            p.summary = (0..80)
                .map(|i| format!("finding {i:02}: a long review line"))
                .collect::<Vec<_>>()
                .join("\n");
            p.options = vec!["retry".into(), "dismiss".into()];
        }
        control(&mut st, &[d.created("delta"), d.turn_started("review"), rv]);
        st.view.compact_width = 60;
        rows(&mut st, 50, 24);
        type_line(&mut st, "/open gamma");
        type_line(&mut st, "/n");
        st.set_focus(Some("t-delta".into()));
        st.handle_key(key(KeyCode::Enter));
        rows(&mut st, 50, 24);
        rows(&mut st, 50, 24);
        assert_eq!(st.view.compact_pane, CompactPane::Chat);
        assert_eq!(st.focused(), Some("t-delta"));
        assert_eq!(st.detail_card(), Some("t-delta"));
        assert!(st.action_popup().is_some(), "the popup keeps its target");
        // Its popup closed to read the detail, Enter opens it again, not the
        // older item of another card.
        st.handle_key(key(KeyCode::Esc));
        rows(&mut st, 50, 24);
        assert!(st.view.action.is_none());
        assert_eq!(st.detail_card(), Some("t-delta"));
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            st.action_popup().map(|p| p.target.clone()),
            Some(ActionTarget::Card("t-delta".into()))
        );
    }

    /// On a list during a dive, Esc releases the row, then leaves the dive,
    /// with draws between.
    #[test]
    fn esc_on_a_compact_list_leaves_the_dive() {
        let mut st = three_threads();
        st.view.compact_width = 60;
        rows(&mut st, 50, 24);
        type_line(&mut st, "/open beta");
        type_line(&mut st, "/t");
        assert!(st.focused().is_some());
        st.handle_key(key(KeyCode::Esc));
        rows(&mut st, 50, 24);
        assert!(st.focused().is_none());
        st.handle_key(key(KeyCode::Esc));
        rows(&mut st, 50, 24);
        assert!(st.view.dive.is_none());
        assert_eq!(st.view.compact_pane, CompactPane::Chat);
    }

    /// A compact list longer than the screen scrolls with the wheel, and the
    /// status bar names the compact layout's own keys.
    #[test]
    fn a_long_compact_list_scrolls_with_the_wheel() {
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        let names: Vec<String> = (0..20).map(|i| format!("w{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut st, _) = working(&refs);
        st.view.compact_width = 60;
        rows(&mut st, 50, 14);
        type_line(&mut st, "/t");
        let lines = rows(&mut st, 50, 14);
        let list = st.view.panes.list.unwrap();
        assert_eq!(list.start, 0);
        assert!(list.max > 0, "{lines:#?}");
        for _ in 0..3 {
            st.handle_mouse(
                mouse(MouseEventKind::ScrollDown, list.area.x + 1, list.area.y + 1),
                &screen,
            );
            rows(&mut st, 50, 14);
        }
        assert_eq!(st.view.panes.list.unwrap().start, 3);
        assert!(crate::ui::hints(&st).starts_with("↑↓/tap select"));
        type_line(&mut st, "/c");
        assert!(crate::ui::hints(&st).contains("/n needs you"));
        assert!(!crate::ui::hints(&st).contains("Ctrl-G"));
    }

    /// The compact layout coming back on a list leaves nothing of the
    /// conversation selected; its rows end in `…` only when cut.
    #[test]
    fn a_compact_list_coming_back_selects_a_row() {
        let mut st = three_threads();
        st.view.compact_width = 60;
        rows(&mut st, 50, 24);
        type_line(&mut st, "/n");
        let first = st.focused().map(str::to_string);
        type_line(&mut st, "/compact off");
        rows(&mut st, 50, 24);
        st.handle_key(shift(KeyCode::Left));
        assert!(st.view.main_focus);
        type_line(&mut st, "/compact on");
        assert!(!st.view.main_focus);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            st.action_popup().map(|p| p.target.clone()),
            first.map(ActionTarget::Card)
        );
        st.handle_key(key(KeyCode::Esc));
        // Threads: the cards, the selected one drawn as selected; cut with `…` only when cut.
        type_line(&mut st, "/t");
        // (The cards are what is above the input box.)
        let cards = |lines: Vec<String>| -> Vec<String> {
            lines
                .into_iter()
                .take_while(|l| !l.contains("→ orchestrator"))
                .collect()
        };
        let lines = cards(rows(&mut st, 50, 24));
        assert!(lines.iter().any(|l| l.starts_with('┏')), "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains('…')), "{lines:#?}");
        let lines = cards(rows(&mut st, 12, 24));
        assert!(lines.iter().any(|l| l.contains('…')), "{lines:#?}");
    }

    /// A dive with nothing waiting: "Needs you" is empty, and Enter opens
    /// nothing about the hidden dive; the list's screen comes back with the workspace.
    #[test]
    fn an_empty_needs_list_opens_nothing() {
        let mut st = AppState::new();
        let mut a = ThreadScript::new("t-solo");
        control(&mut st, &[a.created("solo"), a.turn_started("work")]);
        st.view.compact_width = 60;
        rows(&mut st, 50, 24);
        type_line(&mut st, "/open solo");
        type_line(&mut st, "/n");
        assert!(st.selection_target().is_none());
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.action.is_none());
        assert_eq!(st.view.status.as_deref(), Some("nothing waits on you"));
    }

    /// `/compact on|off` overrides the width; the screens' commands say what they are for
    /// outside it.
    #[test]
    fn compact_is_switched_by_hand_too() {
        let mut st = three_threads();
        st.view.compact_width = 60;
        rows(&mut st, 120, 30);
        assert!(!st.compact());
        type_line(&mut st, "/n");
        assert!(st.view.status.as_deref().unwrap_or("").contains("/compact"));
        type_line(&mut st, "/compact on");
        let lines = rows(&mut st, 120, 30);
        assert!(st.compact());
        // The tab bar's names, on its middle row.
        assert!(
            lines[1].contains("Chat") && lines[1].contains("Needs you 2"),
            "{lines:#?}"
        );
        type_line(&mut st, "/compact off");
        rows(&mut st, 40, 30);
        assert!(!st.compact());
    }

    pub(crate) fn three_threads() -> AppState {
        let mut st = AppState::new();
        let mut a = ThreadScript::new("t-alpha");
        let mut b = ThreadScript::new("t-beta");
        let mut c = ThreadScript::new("t-gamma");
        let mut evs = vec![a.created("alpha"), a.turn_started("refactor")];
        let mut rv = a.raise("rv1", Review);
        if let EventBody::AttentionRaised(p) = &mut rv.body {
            p.summary = "turn gave up".into();
            p.options = vec!["retry".into(), "dismiss".into()];
        }
        evs.push(rv);
        b.at(10);
        evs.extend([b.created("beta"), b.turn_started("deploy")]);
        let mut perm = b.raise("pm1", Permission);
        if let EventBody::AttentionRaised(p) = &mut perm.body {
            p.summary = "Bash: rm -rf target".into();
        }
        evs.push(perm);
        evs.push(b.annotate("pm1", "build dir only"));
        c.at(20);
        evs.extend([c.created("gamma"), c.turn_started("tests")]);
        control(&mut st, &evs);
        st
    }

    /// A deleted thread leaves the listing; the status bar says so, and names the directory git
    /// could not remove all of.
    #[test]
    fn a_deleted_thread_goes_and_the_status_names_what_is_left() {
        let mut st = AppState::new();
        let (mut x, mut y) = (ThreadScript::new("x"), ThreadScript::new("y"));
        control(&mut st, &[x.created("x"), y.created("y")]);
        let deleted = |s: &mut ThreadScript, name: &str, left: Option<&str>| {
            s.event(EventBody::ThreadDeleted(twr_core::event::ThreadDeleted {
                name: name.into(),
                worktree_left: left.map(str::to_string),
                trash: left.map(|_| "01J0000000000000000ABCDEFGH".to_string()),
                ..Default::default()
            }))
        };
        control(&mut st, &[deleted(&mut x, "x", None)]);
        assert_eq!(st.order(), ["y"]);
        assert_eq!(st.view.status.as_deref(), Some("deleted x"));
        control(&mut st, &[deleted(&mut y, "y", Some("/w/y"))]);
        assert!(st.order().is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some(
                "deleted y; in the trash as ABCDEFGH (/trash restore ABCDEFGH brings it back); \
                 part of /w/y could not be moved (no worktree any more): remove it by hand"
            )
        );
        // Restored: back in the listing, and the status says so.
        let restored = y.event(EventBody::ThreadRestored(twr_core::event::ThreadRestored {
            item: "01J0000000000000000ABCDEFGH".into(),
            name: "y2".into(),
            previous_name: Some("y".into()),
            ..Default::default()
        }));
        control(&mut st, &[restored]);
        let back = st.registry.get("y").unwrap();
        assert_eq!((back.name.as_str(), back.parked), ("y2", true));
        assert_eq!(
            st.view.status.as_deref(),
            Some("y2 is back from the trash (parked: /resume y2 continues it)")
        );
    }

    /// A deleted thread's transcript is not kept: neither one kept from an earlier visit nor
    /// the one on screen when it was deleted (the main pane leaves it). An `@` mention no
    /// longer offers it; a parked thread, hidden from the cards, it still offers, after the
    /// live ones (like a stopped one).
    #[test]
    fn a_deleted_thread_leaves_the_kept_transcripts_and_the_mentions() {
        let mut st = AppState::new();
        let mut s: Vec<ThreadScript> = ["x", "y", "parked", "live"]
            .into_iter()
            .map(ThreadScript::new)
            .collect();
        let created: Vec<Event> = s
            .iter_mut()
            .zip(["x", "y", "parked", "live"])
            .map(|(t, n)| t.created(n))
            .collect();
        control(&mut st, &created);
        let parked = s[2].event(EventBody::ThreadParked(Default::default()));
        control(&mut st, &[parked]);
        let deleted = |t: &mut ThreadScript, name: &str| {
            t.event(EventBody::ThreadDeleted(twr_core::event::ThreadDeleted {
                name: name.into(),
                ..Default::default()
            }))
        };
        let kept = |st: &AppState, id: &str| st.transcripts.iter().any(|t| t.thread == id);
        // x shown, then y: x's transcript is kept, until x is deleted.
        st.view.dive = Some("x".into());
        st.sync_main();
        st.view.dive = Some("y".into());
        st.sync_main();
        assert!(kept(&st, "x"));
        let ev = deleted(&mut s[0], "x");
        control(&mut st, &[ev]);
        assert!(!kept(&st, "x"));
        // y on screen when deleted: the dive ends, and its transcript is not kept.
        assert_eq!(st.transcript.as_ref().map(|t| t.thread.as_str()), Some("y"));
        let ev = deleted(&mut s[1], "y");
        control(&mut st, &[ev]);
        assert_eq!(st.view.dive, None);
        assert!(!kept(&st, "y"));
        assert!(st.transcript.as_ref().is_none_or(|t| t.thread != "y"));
        let names: Vec<String> = slash::mention("@", 1, &st.pools())
            .items
            .into_iter()
            .map(|i| i.value)
            .collect();
        assert_eq!(names, ["@live", "@parked"]);
        assert_eq!(st.hidden_parked(), ["parked"]);
    }

    /// Nothing is selected until the user selects a card; a card selected stays selected as
    /// others arrive and the stack reorders.
    #[test]
    fn nothing_is_selected_at_start_and_a_selection_stays_put() {
        let mut st = three_threads();
        assert_eq!(st.order(), ["t-beta", "t-alpha", "t-gamma"]);
        assert_eq!(st.focused(), None);
        st.set_focus(Some("t-alpha".into()));
        let mut d = ThreadScript::new("t-delta");
        d.at(50);
        control(&mut st, &[d.created("delta"), d.raise("pm9", Permission)]);
        assert!(st.order().contains(&"t-delta".to_string()));
        assert_eq!(st.focused(), Some("t-alpha"));
        let mut empty = AppState::new();
        let mut s = ThreadScript::new("x");
        control(&mut empty, &[s.created("x")]);
        assert_eq!(empty.focused(), None);
    }

    #[test]
    fn focus_mode_tracks_work_completion_attention_and_failures() {
        use twr_core::event::Grade;
        let mut st = AppState::new();
        let mut w = ThreadScript::new("worker");
        control(&mut st, &[w.created("worker")]);
        let toggle = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        st.handle_key(toggle);
        assert!(st.view.focus_mode);
        assert!(st.order().is_empty(), "an idle thread is hidden");
        assert_eq!(st.focused(), None);
        assert_eq!(st.hidden_in_focus(), 1);
        assert!(st.hidden_parked().is_empty(), "idle is not parked");

        control(&mut st, &[w.turn_started("work")]);
        assert_eq!(st.order(), ["worker"]);
        st.set_focus(Some("worker".into()));
        control(&mut st, &[w.turn_ended(TurnResult::Completed)]);
        assert!(st.order().is_empty(), "success hides before the report too");
        assert_eq!(st.focused(), None, "a card that went is not selected");
        control(&mut st, &[w.report(Some(1), Grade::Success)]);
        assert!(st.order().is_empty());

        control(&mut st, &[w.raise("review", Review)]);
        assert_eq!(st.order(), ["worker"], "an open review brings it back");
        control(&mut st, &[w.resolve("review")]);
        assert!(st.order().is_empty());
        control(
            &mut st,
            &[w.turn_started("again"), w.turn_ended(TurnResult::Error)],
        );
        assert_eq!(st.order(), ["worker"], "errors stay even without a card");
        control(
            &mut st,
            &[w.turn_started("fix"), w.raise("permission", Permission)],
        );
        assert_eq!(st.order(), ["worker"]);
        control(
            &mut st,
            &[w.resolve("permission"), w.turn_ended(TurnResult::Completed)],
        );
        assert!(st.order().is_empty());
        control(&mut st, &[w.attached()]);
        assert_eq!(st.order(), ["worker"], "attached CLI sessions stay visible");
        control(&mut st, &[w.detached()]);
        assert!(st.order().is_empty());
        st.handle_key(toggle);
        assert!(!st.view.focus_mode);
        assert_eq!(st.order(), ["worker"]);
        assert_eq!(st.focused(), None);
    }

    #[test]
    fn focus_mode_keeps_pins_dives_and_explicitly_listed_parked_threads() {
        let mut st = with_parked_zeta();
        st.set_focus(Some("t-eps".into()));
        st.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(!st.order().contains(&"t-eps".to_string()));
        assert_ne!(st.focused(), Some("t-eps"));
        assert_eq!(st.hidden_in_focus(), 1);
        assert_eq!(st.hidden_parked(), ["zeta"]);
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        assert!(!st.grid_order().contains(&"t-eps".to_string()));
        assert!(st.nav_order().iter().all(|id| st.order().contains(id)));
        assert_eq!(st.thread_arg(Some("eps")).unwrap().id, "t-eps");
        assert!(st.pools().mentions.iter().any(|t| t.name == "eps"));

        type_line(&mut st, "/open eps");
        assert_eq!(st.view.dive.as_deref(), Some("t-eps"));
        assert!(st.order().contains(&"t-eps".to_string()));
        assert_eq!(st.hidden_in_focus(), 0);
        type_line(&mut st, "/back");
        assert!(!st.order().contains(&"t-eps".to_string()));
        st.view.pins.insert("t-eps".into());
        assert!(st.order().contains(&"t-eps".to_string()));
        st.set_focus(Some("t-eps".into()));
        type_line(&mut st, "/pin");
        assert_eq!(st.view.status.as_deref(), Some("unpinned eps"));
        assert!(!st.order().contains(&"t-eps".to_string()));
        assert_ne!(st.focused(), Some("t-eps"));

        type_line(&mut st, "/parked on");
        assert!(st.order().contains(&"t-zeta".to_string()));
        type_line(&mut st, "/parked off");
        assert!(!st.order().contains(&"t-zeta".to_string()));
    }

    #[test]
    fn sidebar_and_focus_shortcuts_preserve_input_and_commands_set_the_views() {
        let mut st = five_threads();
        st.set_focus(Some("t-alpha".into()));
        st.view.input.set("/open ep");
        let sidebar = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let focus = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(st.popup().is_some());
        st.handle_key(sidebar);
        st.handle_key(focus);
        assert_eq!(st.view.input.as_str(), "/open ep");
        assert!(st.view.sidebar_hidden && st.view.focus_mode);
        assert!(st.stack_nav().is_empty());
        assert_eq!(st.focused(), None);

        type_line(&mut st, "/sidebar on");
        assert!(!st.view.sidebar_hidden);
        assert_eq!(
            st.focused(),
            Some("t-alpha"),
            "the selected card is restored"
        );
        type_line(&mut st, "/focus off");
        assert!(!st.view.focus_mode);
        type_line(&mut st, "/focus");
        assert!(st.view.focus_mode);
        type_line(&mut st, "/sidebar off");
        assert!(st.view.sidebar_hidden);
        type_line(&mut st, "/sidebar");
        assert!(!st.view.sidebar_hidden);
        type_line(&mut st, "/focus off");
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        st.handle_key(sidebar);
        assert_eq!(st.view.screen, Screen::Stack);
        assert!(st.view.sidebar_hidden);
    }

    /// Nothing is selected after the initial replay either (a card picked during it stays);
    /// Tab then selects the leftmost card, which live events that reorder the stack keep.
    #[test]
    fn after_the_initial_replay_nothing_is_selected_until_tab() {
        // alpha (review), beta (permission), gamma (running): stack beta, alpha, gamma.
        let mut a = ThreadScript::new("t-alpha");
        let mut b = ThreadScript::new("t-beta");
        let mut c = ThreadScript::new("t-gamma");
        let mut evs = vec![a.created("alpha"), a.turn_started("refactor")];
        evs.push(a.raise("rv1", Review));
        b.at(10);
        evs.extend([b.created("beta"), b.turn_started("deploy")]);
        evs.push(b.raise("pm1", Permission));
        c.at(20);
        evs.extend([c.created("gamma"), c.turn_started("tests")]);
        let head = evs.len() as u64;

        let mut st = AppState::new();
        st.begin_catch_up(head);
        control(&mut st, &evs[..7]);
        assert_eq!(st.focused(), None, "no focus while replaying");
        control(&mut st, &evs[7..]);
        assert_eq!(st.order(), ["t-beta", "t-alpha", "t-gamma"]);
        assert_eq!(st.focused(), None, "none after it either");
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-beta"), "Tab: the leftmost card");
        // Live events that reorder the stack keep the focus.
        c.at(30);
        control(&mut st, &[c.raise("pm2", Permission)]);
        assert_eq!(st.order(), ["t-beta", "t-gamma", "t-alpha"]);
        assert_eq!(st.focused(), Some("t-beta"));
        // So does a focus the user picked.
        st.handle_key(key(KeyCode::Tab));
        let picked = st.focused().unwrap().to_string();
        assert_ne!(picked, "t-beta");
        a.at(40);
        control(&mut st, &[a.raise("pm3", Permission)]);
        assert_eq!(st.focused(), Some(picked.as_str()));

        // A focus picked during the replay survives its end.
        let mut st = AppState::new();
        st.begin_catch_up(head);
        control(&mut st, &evs[..7]);
        st.set_focus(Some("t-alpha".into()));
        control(&mut st, &evs[7..]);
        assert_eq!(st.focused(), Some("t-alpha"));

        // Nothing to replay: still nothing selected.
        let mut st = AppState::new();
        st.begin_catch_up(0);
        let mut x = ThreadScript::new("x");
        control(&mut st, &[x.created("x")]);
        assert_eq!(st.focused(), None);
    }

    #[test]
    fn focus_movement_wraps_in_stack_order() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-alpha"));
        st.handle_key(shift(KeyCode::Down));
        assert_eq!(st.focused(), Some("t-gamma"));
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-beta"));
        st.handle_key(shift(KeyCode::BackTab));
        assert_eq!(st.focused(), Some("t-gamma"));
        st.handle_key(shift(KeyCode::Up));
        assert_eq!(st.focused(), Some("t-alpha"));
        // Plain ↑↓ are the input line's: they move no selection.
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.focused(), Some("t-alpha"));
    }

    /// The `thread.created` (role orchestrator, runtime claude-code, model asked `opus`) and
    /// `thread.started` (reporting `claude-opus-4-1-20250805`) of orchestrator `id`.
    pub(crate) fn orch_events(o: &mut ThreadScript, id: &str) -> Vec<Event> {
        let mut created = o.created(id.trim_start_matches("t-"));
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.runtime = "claude-code".into();
            p.ext = serde_json::json!({"twr": {"role": "orchestrator", "model": "opus"}})
                .as_object()
                .cloned();
        }
        let mut started = o.started("s-orch");
        if let EventBody::ThreadStarted(p) = &mut started.body {
            p.ext = serde_json::json!({"claude": {"model": "claude-opus-4-1-20250805"}})
                .as_object()
                .cloned();
        }
        vec![created, started]
    }

    /// [`three_threads`] plus the orchestrator `t-orch` (named `orch`).
    pub(crate) fn with_orch() -> AppState {
        let mut st = three_threads();
        let mut o = ThreadScript::new("t-orch");
        o.at(40);
        let evs = orch_events(&mut o, "t-orch");
        control(&mut st, &evs);
        st
    }

    /// The main pane is the orchestrator's conversation whatever card is selected: the
    /// orchestrator is no card, selecting a card changes nothing there, and a typed line goes
    /// to the orchestrator.
    #[test]
    fn the_main_pane_is_the_orchestrator_whatever_card_is_selected() {
        let mut st = three_threads();
        let mut o = ThreadScript::new("t-orch");
        o.at(40);
        let evs = orch_events(&mut o, "t-orch");
        control(&mut st, &evs);
        for i in 0..30 {
            st.apply_event(&thread_item(&o.text(&format!("reply {i}"))));
        }
        assert_eq!(st.order(), ["t-beta", "t-alpha", "t-gamma"]);
        assert!(!st.grid_order().contains(&"t-orch".to_string()));
        assert_eq!(st.main_thread().as_deref(), Some("t-orch"));
        assert_eq!(st.transcript.as_ref().unwrap().thread, "t-orch");
        draw(&mut st, 160, 40);
        st.handle_key(key(KeyCode::PageUp));
        draw(&mut st, 160, 40);
        let scrolled = (st.view.scroll, st.view.anchor.clone());
        assert!(scrolled.0 > 0, "{scrolled:?}");
        for _ in 0..4 {
            st.handle_key(key(KeyCode::Tab));
            assert_ne!(st.focused(), Some("t-orch"));
            assert_eq!(st.transcript.as_ref().unwrap().thread, "t-orch");
            draw(&mut st, 160, 40);
        }
        assert_eq!(
            (st.view.scroll, st.view.anchor.clone()),
            scrolled,
            "selecting a card leaves the main pane alone"
        );
        assert_eq!(
            type_line(&mut st, "deploy the api"),
            [Action::Say {
                text: "deploy the api".into(),
                seen_gseq: st.last_gseq,
            }]
        );
        // Before the orchestrator exists: nothing to show, and still a `say`.
        let st0 = three_threads();
        assert_eq!(st0.main_thread(), None);
        assert!(st0.transcript.is_none());
    }

    /// With one worker thread, a typed line still goes to the orchestrator (`say` with `to:
    /// "orchestrator"`), never to the only thread.
    #[test]
    fn one_worker_is_no_implicit_target() {
        let mut st = AppState::new();
        let mut w = ThreadScript::new("t-web");
        let mut o = ThreadScript::new("t-orch");
        o.at(5);
        let mut evs = vec![w.created("web"), w.turn_started("build")];
        evs.extend(orch_events(&mut o, "t-orch"));
        control(&mut st, &evs);
        assert_eq!(st.order(), ["t-web"]);
        st.set_focus(Some("t-web".into()));
        let actions = type_line(&mut st, "how is web doing?");
        assert_eq!(
            actions,
            [Action::Say {
                text: "how is web doing?".into(),
                seen_gseq: st.last_gseq,
            }]
        );
        let (m, p) = crate::action_request(&actions[0]).unwrap();
        assert_eq!(m, "say");
        assert_eq!(p["to"], "orchestrator");
        assert_eq!(p["seen_gseq"], st.last_gseq);
        assert!(p.get("focused").is_none(), "{p}");
    }

    /// Enter, Enter on a worker's card dives in (the first opens its action popup, whose Talk
    /// row is highlighted); the dive selects nothing. Esc leaves it with that card selected
    /// again: so do `/back`, and Esc after diving on into another thread.
    #[test]
    fn enter_enter_dives_and_esc_selects_the_card_again() {
        let mut st = with_orch();
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.action.is_some(), "the first Enter opens the popup");
        assert_eq!(st.view.dive, None);
        drawn(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        assert_eq!(st.view.action, None);
        assert_eq!(st.focused(), None, "the dive selects nothing");
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (st.view.dive.as_deref(), st.focused()),
            (None, Some("t-beta"))
        );

        st.handle_key(key(KeyCode::Enter));

        drawn(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert!(type_line(&mut st, "/back").is_empty());
        assert_eq!(
            (st.view.dive.as_deref(), st.focused()),
            (None, Some("t-beta"))
        );

        st.handle_key(key(KeyCode::Enter));

        drawn(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert!(type_line(&mut st, "/open gamma").is_empty());
        assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (st.view.dive.as_deref(), st.focused()),
            (None, Some("t-beta"))
        );
    }

    /// In a dive, Esc first releases a card selected meanwhile, then leaves the dive; what is
    /// typed stays through both, and goes to the orchestrator after.
    #[test]
    fn in_a_dive_esc_releases_the_card_then_leaves() {
        let mut st = with_orch();
        assert!(type_line(&mut st, "/open beta").is_empty());
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-beta"));
        typed(&mut st, "thanks");
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (st.view.dive.as_deref(), st.focused()),
            (Some("t-beta"), None)
        );
        st.handle_key(key(KeyCode::Esc));
        assert_eq!((st.view.dive.as_deref(), st.focused()), (None, None));
        assert_eq!(st.view.input.as_str(), "thanks");
        assert!(matches!(
            st.handle_key(key(KeyCode::Enter)).as_slice(),
            [Action::Say { .. }]
        ));
    }

    /// A dive the hub ends (here the reply to `/clear`) keeps what is typed and restores
    /// nothing later: the next Esc has nothing to release or leave.
    #[test]
    fn a_dive_the_hub_ends_keeps_the_line_typed() {
        let mut st = with_orch();
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        typed(&mut st, "draft");
        st.apply_result("orchestrator.reset", &serde_json::json!({}));
        assert_eq!(st.view.dive, None);
        assert_eq!(st.view.focus_before_dive, None);
        st.handle_key(key(KeyCode::Esc));
        assert_eq!((st.view.dive.as_deref(), st.focused()), (None, None));
        assert_eq!(st.view.input.as_str(), "draft");
    }

    /// Enter, Enter on a worker's card dives in: the main pane shows it, typed lines go to it
    /// with `thread.send`; Esc (or `/back`) returns to the orchestrator.
    #[test]
    fn diving_in_sends_to_the_thread_and_esc_returns() {
        let mut st = with_orch();
        st.set_focus(Some("t-beta".into()));
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        assert_eq!(st.transcript.as_ref().unwrap().thread, "t-beta");
        for c in "use the staging db".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        let actions = st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            actions,
            [Action::Send {
                thread: "t-beta".into(),
                text: "use the staging db".into(),
            }]
        );
        assert_eq!(
            crate::action_request(&actions[0]),
            Some((
                "thread.send",
                serde_json::json!({"thread": "t-beta", "prompt": "use the staging db"})
            ))
        );
        // Selecting another card does not change who a line goes to.
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(
            type_line(&mut st, "and rerun"),
            [Action::Send {
                thread: "t-beta".into(),
                text: "and rerun".into(),
            }]
        );
        // Esc releases the card selected, then returns.
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None);
        assert_eq!(st.transcript.as_ref().unwrap().thread, "t-orch");
        assert!(matches!(
            type_line(&mut st, "thanks").as_slice(),
            [Action::Say { .. }]
        ));
        // /open and /back; the orchestrator cannot be dived into.
        assert!(type_line(&mut st, "/open gamma").is_empty());
        assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
        assert_eq!(st.focused(), None);
        assert!(type_line(&mut st, "/back").is_empty());
        assert_eq!(st.view.dive, None);
        assert!(type_line(&mut st, "/open orch").is_empty());
        assert_eq!(st.view.dive, None);
        assert_eq!(
            st.view.status.as_deref(),
            Some("the orchestrator is the main conversation")
        );
        assert!(type_line(&mut st, "/open nope").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no thread nope"));
        // /open without a thread: the selected card.
        st.set_focus(Some("t-alpha".into()));
        assert!(type_line(&mut st, "/open").is_empty());
        assert_eq!(st.view.dive.as_deref(), Some("t-alpha"));
        // Esc returns, the card selected again; Ctrl-G (leaving the stack view) ends a dive.
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (st.view.dive.as_deref(), st.focused()),
            (None, Some("t-alpha"))
        );
        st.dive("t-alpha");
        st.handle_key(ctrl('g'));
        assert_eq!(
            (st.view.dive.as_deref(), st.view.screen),
            (None, Screen::Grid)
        );
        // The hub's card has no thread to talk to: its popup has no Talk row.
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(90);
        control(&mut st, &[h.raise("hq9", Question)]);
        st.view.screen = Screen::Stack;
        st.set_focus(Some(HUB_THREAD.into()));
        st.handle_key(key(KeyCode::Enter));
        let target = st.view.action.as_ref().expect("its popup").target.clone();
        assert!(st
            .action_rows(&target)
            .iter()
            .all(|r| r.kind != ActionKind::Talk));
        assert_eq!(st.view.dive, None);
    }

    /// After a rename, slash commands still name the thread by its old name, as the hub does;
    /// a former name two threads had names neither, and a prefix names nothing.
    #[test]
    fn slash_commands_take_a_threads_former_names() {
        let mut st = with_orch();
        let rename = |s: &mut ThreadScript, to: &str| {
            s.event(EventBody::ThreadRenamed(twr_core::event::ThreadRenamed {
                name: to.into(),
                by: ChangedBy::User,
                ..Default::default()
            }))
        };
        let mut b = ThreadScript::new("t-beta");
        b.at(50);
        control(&mut st, &[rename(&mut b, "gateway")]);
        assert_eq!(st.registry.get("t-beta").unwrap().former_names, ["beta"]);
        assert_eq!(st.thread_arg(Some("beta")).unwrap().id, "t-beta");
        assert_eq!(
            type_line(&mut st, "/rename beta billing"),
            [Action::Rename {
                thread: "t-beta".into(),
                name: "billing".into(),
            }]
        );
        assert_eq!(
            type_line(&mut st, "/mode beta plan"),
            [Action::SetMode {
                thread: "t-beta".into(),
                mode: "plan".into(),
            }]
        );
        type_line(&mut st, "/open beta");
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        type_line(&mut st, "/back");
        assert_eq!(st.thread_arg(Some("bet")).unwrap_err(), "no thread bet");
        // alpha and gamma were both called `shared` once.
        let mut a = ThreadScript::new("t-alpha");
        let mut c = ThreadScript::new("t-gamma");
        a.at(60);
        c.at(61);
        control(
            &mut st,
            &[
                rename(&mut a, "shared"),
                rename(&mut a, "a2"),
                rename(&mut c, "shared"),
                rename(&mut c, "g2"),
            ],
        );
        let e = st.thread_arg(Some("shared")).unwrap_err();
        assert!(e.contains("names several threads"), "{e}");
        assert!(type_line(&mut st, "/mode shared plan").is_empty());
        assert_eq!(st.thread_arg(Some("alpha")).unwrap().id, "t-alpha");
    }

    /// `/rename <name>` in a dive renames that thread; in the conversation it takes the thread
    /// first (`thread.rename`). Once renamed, the transcript says so.
    #[test]
    fn rename_renames_a_thread() {
        let mut st = with_orch();
        assert!(type_line(&mut st, "/rename gateway").is_empty());
        assert!(
            st.view
                .status
                .as_deref()
                .unwrap()
                .starts_with("usage: /rename <thread> <name>"),
            "{:?}",
            st.view.status
        );
        let actions = type_line(&mut st, "/rename beta gateway");
        assert_eq!(
            actions,
            [Action::Rename {
                thread: "t-beta".into(),
                name: "gateway".into(),
            }]
        );
        assert_eq!(
            crate::action_request(&actions[0]),
            Some((
                "thread.rename",
                serde_json::json!({"thread": "t-beta", "name": "gateway"})
            ))
        );
        assert_eq!(
            st.view.status.as_deref(),
            Some("renaming beta → gateway: waiting for the hub")
        );
        st.dive("t-gamma");
        assert_eq!(
            type_line(&mut st, "/rename gamma-ui"),
            [Action::Rename {
                thread: "t-gamma".into(),
                name: "gamma-ui".into(),
            }]
        );
        let mut tr = Transcript::new("t-gamma");
        tr.apply(
            &ThreadScript::new("t-gamma").event(EventBody::ThreadRenamed(
                twr_core::event::ThreadRenamed {
                    name: "gamma-ui".into(),
                    previous: "gamma".into(),
                    by: ChangedBy::Orchestrator,
                    ..Default::default()
                },
            )),
        );
        assert!(
            tr.entries
                .iter()
                .any(|e| matches!(e, Entry::Notice { text, .. }
                if text == "renamed gamma → gamma-ui by the orchestrator")),
            "{:?}",
            tr.entries
        );
    }

    /// `/mode <mode>` in a dive sets that thread's mode; in the conversation it takes the
    /// thread first (`thread.set_mode`).
    #[test]
    fn mode_sets_a_threads_permission_mode() {
        let mut st = with_orch();
        assert!(type_line(&mut st, "/mode auto").is_empty());
        assert!(
            st.view
                .status
                .as_deref()
                .unwrap()
                .starts_with("usage: /mode <thread> <mode>"),
            "{:?}",
            st.view.status
        );
        assert_eq!(
            st.view.input, "/mode auto",
            "a failed command stays on the line"
        );
        let actions = type_line(&mut st, "/mode beta acceptEdits");
        assert_eq!(
            actions,
            [Action::SetMode {
                thread: "t-beta".into(),
                mode: "acceptEdits".into(),
            }]
        );
        assert_eq!(
            crate::action_request(&actions[0]),
            Some((
                "thread.set_mode",
                serde_json::json!({"thread": "t-beta", "mode": "acceptEdits"})
            ))
        );
        assert_eq!(
            st.view.status.as_deref(),
            Some("mode of beta → acceptEdits: waiting for the hub")
        );
        st.dive("t-gamma");
        assert_eq!(
            type_line(&mut st, "/mode plan"),
            [Action::SetMode {
                thread: "t-gamma".into(),
                mode: "plan".into(),
            }]
        );
        // Naming a thread works in a dive too.
        assert_eq!(
            type_line(&mut st, "/mode alpha auto"),
            [Action::SetMode {
                thread: "t-alpha".into(),
                mode: "auto".into(),
            }]
        );
        assert!(type_line(&mut st, "/mode nope auto").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no thread nope"));
        assert!(type_line(&mut st, "/mode").is_empty());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .starts_with("usage: /mode"));
        // The hub's answer: the updated thread.
        assert_eq!(
            crate::result_status(
                "thread.set_mode",
                &serde_json::json!({"thread": "t-gamma", "thread_name": "gamma",
                                    "mode": "plan", "changed": true, "applies": "next_start"})
            )
            .as_deref(),
            Some("gamma is now in mode plan; from its next start")
        );
    }

    /// One worker `w` with nothing open, and the orchestrator `t-orch` with a permission
    /// `op1` (allow, deny) on its banner.
    fn worker_and_orch_item() -> (AppState, ThreadScript) {
        let mut st = AppState::new();
        let mut w = ThreadScript::new("t-w");
        let mut evs = vec![w.created("w"), w.turn_started("work")];
        let mut o = ThreadScript::new("t-orch");
        o.at(40);
        evs.extend(orch_events(&mut o, "t-orch"));
        let mut p = o.raise("op1", Permission);
        if let EventBody::AttentionRaised(r) = &mut p.body {
            r.summary = "Bash: git push --force".into();
            r.options = vec!["allow".into(), "deny".into()];
        }
        evs.push(p);
        control(&mut st, &evs);
        (st, o)
    }

    fn answers_op1(actions: &[Action]) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, Action::Respond { attn_id, .. } if attn_id == "op1"))
    }

    /// Going back to a thread shows its transcript as it was, at once: the transcript is
    /// kept while the main pane shows another thread, and the subscription made on return
    /// resumes after its last seq, so only what came after it is added.
    #[test]
    fn a_thread_shown_again_keeps_its_transcript() {
        let (mut st, mut o) = worker_and_orch_item();
        let orch = [
            o.turn_started("hi"),
            o.text("hello"),
            o.turn_ended(TurnResult::Completed),
        ];
        let head = orch[2].seq;
        // The first subscription replays the orchestrator's log up to its head: it is
        // catching up (the TUI draws nothing per event) until the head is applied.
        st.transcript.as_mut().unwrap().resume(0, head);
        for ev in &orch {
            assert!(st.transcript_catching_up());
            st.apply_event(&thread_item(ev));
        }
        assert!(!st.transcript_catching_up());
        let seen = st.transcript.clone().unwrap();
        assert_eq!(seen.entries.len(), 3);

        st.dive("t-w");
        assert_eq!(st.transcript.as_ref().unwrap().thread, "t-w");
        assert!(st.transcript.as_ref().unwrap().entries.is_empty());
        let mut w = ThreadScript::new("t-w");
        let work = [w.turn_started("work"), w.text("working")];
        st.transcript.as_mut().unwrap().resume(0, work[1].seq);
        for ev in &work {
            st.apply_event(&thread_item(ev));
        }

        // Back: the orchestrator's transcript is the one left, before any event arrives.
        st.back();
        let tr = st.transcript.as_ref().unwrap();
        assert_eq!((&tr.entries, tr.last_seq), (&seen.entries, head));
        assert!(!st.transcript_catching_up());
        // Its subscription starts right after what it holds; the hub replays from there.
        let later = o.text("and more");
        let since = crate::resume_since(head, later.seq);
        assert_eq!(since, head);
        st.transcript.as_mut().unwrap().resume(since, later.seq);
        assert!(st.transcript_catching_up());
        st.apply_event(&thread_item(&orch[1]));
        st.apply_event(&thread_item(&later));
        assert!(!st.transcript_catching_up());
        let tr = st.transcript.as_ref().unwrap();
        assert_eq!(tr.entries[..3], seen.entries[..]);
        assert_eq!(
            tr.entries[3..],
            [Entry::Text {
                seq: later.seq,
                text: "and more".into(),
                open: false
            }]
        );

        // The worker's transcript was kept too.
        st.dive("t-w");
        let tr = st.transcript.as_ref().unwrap();
        assert_eq!((tr.entries.len(), tr.last_seq), (2, work[1].seq));
    }

    /// A kept transcript the new subscription cannot continue starts over: events between
    /// were never seen (the replay starts past its end), or the log is shorter than it.
    #[test]
    fn a_kept_transcript_that_cannot_be_continued_starts_over() {
        let mut s = ThreadScript::new("t1");
        let evs = [s.turn_started("go"), s.text("done")];
        let filled = || {
            let mut tr = Transcript::new("t1");
            for ev in &evs {
                tr.apply(ev);
            }
            tr
        };
        let mut tr = filled();
        tr.resume(500, 2600);
        assert_eq!(
            tr.entries,
            [Entry::Notice {
                seq: 500,
                level: NoticeLevel::Info,
                text: "earlier events not shown (from seq 501)".into()
            }]
        );
        assert_eq!(tr.last_seq, 500);
        assert!(tr.catching_up());
        let mut tr = filled();
        tr.resume(0, 1);
        assert!(tr.entries.is_empty());
        assert_eq!(tr.last_seq, 0);
        // Continued: kept, nothing to replay.
        let mut tr = filled();
        let last = tr.last_seq;
        tr.resume(last, last);
        assert_eq!(
            tr,
            Transcript {
                catch_up: Some(last),
                ..filled()
            }
        );
        assert!(!tr.catching_up());
    }

    /// The kept transcripts are bounded: the ones shown longest ago are dropped.
    #[test]
    fn kept_transcripts_are_bounded() {
        let mut st = AppState::new();
        let n = TRANSCRIPT_CACHE + 3;
        for i in 0..n {
            st.view.dive = Some(format!("t{i}"));
            st.sync_main();
        }
        let kept: Vec<&str> = st.transcripts.iter().map(|t| t.thread.as_str()).collect();
        let want: Vec<String> = (n - 1 - TRANSCRIPT_CACHE..n - 1)
            .map(|i| format!("t{i}"))
            .collect();
        assert_eq!(kept, want);
        // Shown again, one leaves the kept ones and the one it replaces joins them.
        st.view.dive = Some(format!("t{}", n - 2));
        st.sync_main();
        assert_eq!(st.transcripts.len(), TRANSCRIPT_CACHE);
        assert_eq!(st.transcripts.last().unwrap().thread, format!("t{}", n - 1));
        assert!(st
            .transcripts
            .iter()
            .all(|t| t.thread != format!("t{}", n - 2)));
    }

    /// Review: during a worker dive the orchestrator's banner is not drawn, yet Tab selected
    /// it and 1 allowed it. In a dive the banner is never selected, its action popup never
    /// opens (a stale selection of it opens none, and digits then type), and what opens
    /// answers only what the last frame drew with its options; back in the main conversation
    /// the banner is answered once its popup has been on screen long enough.
    #[test]
    fn a_dive_never_selects_or_answers_the_hidden_banner() {
        let (mut st, _) = worker_and_orch_item();
        assert_eq!(st.stack_nav(), ["t-orch", "t-w"]);
        st.dive("t-w");
        assert_eq!(st.nav_order(), ["t-w"]);
        for _ in 0..3 {
            st.handle_key(key(KeyCode::Tab));
            assert_eq!(st.focused(), Some("t-w"));
            popup_open(&mut st);
            assert!(!answers_op1(&st.handle_key(key(KeyCode::Char('1')))));
            st.handle_key(key(KeyCode::Esc));
            st.handle_key(shift(KeyCode::BackTab));
            assert_eq!(st.focused(), Some("t-w"));
        }
        // Even selected (a stale selection), the banner the dive hides answers nothing.
        st.set_focus(Some("t-orch".into()));
        settled(&mut st);
        assert_eq!(st.view.shown, None);
        assert!(st.view.panes.drawn_on("t-orch").is_none());
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None, "no popup for what is not drawn");
        for c in ['1', '2'] {
            assert!(st.handle_key(key(KeyCode::Char(c))).is_empty(), "{c}");
        }
        assert_eq!(st.view.input.as_str(), "12", "digits type");
        st.view.input.clear();
        // Back in the main conversation (Esc releases the card, then leaves) the banner is
        // drawn and selectable; its popup answers it only once on screen long enough.
        st.handle_key(key(KeyCode::Esc));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None);
        st.set_focus(Some("t-w".into()));
        st.handle_key(shift(KeyCode::BackTab));
        assert_eq!(st.focused(), Some("t-orch"));
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "op1".into(),
                revision: revision(&st, "op1"),
                decision: "allow".into()
            }]
        );
        // Diving releases the selection.
        st.dive("t-w");
        assert_eq!(st.focused(), None);
    }

    /// Review: the grid does not draw the orchestrator's banner, yet a selection kept from
    /// the stack view could answer it. Switching views drops a selection the view does not
    /// draw, Tab never reaches the banner there, and its popup never opens.
    #[test]
    fn the_grid_never_answers_the_hidden_banner() {
        let (mut st, _) = worker_and_orch_item();
        st.set_focus(Some("t-orch".into()));
        settled(&mut st);
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        assert_eq!(st.focused(), None);
        for _ in 0..3 {
            st.handle_key(key(KeyCode::Tab));
            assert_eq!(st.focused(), Some("t-w"));
        }
        popup_open(&mut st);
        assert!(!answers_op1(&st.handle_key(key(KeyCode::Char('1')))));
        st.handle_key(key(KeyCode::Esc));
        // Even selected, it is not on screen: no popup.
        st.set_focus(Some("t-orch".into()));
        settled(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None);
        // The decisions overlay draws no card: Tab selects nothing there.
        st.set_focus(Some("t-w".into()));
        type_line(&mut st, "/decisions");
        assert_eq!(st.view.screen, Screen::Decisions);
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-w"));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.screen, Screen::Grid);
        // Back in the stack view: the banner is drawn again and answered after the settle.
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Stack);
        st.handle_key(shift(KeyCode::BackTab));
        assert_eq!(st.focused(), Some("t-orch"));
        assert!(answers_op1(&answer_with(&mut st, 2)));
    }

    /// The orchestrator's own items (an error the hub raised about it) show on top of its
    /// conversation, selectable like a card, never as a card.
    #[test]
    fn the_orchestrators_items_are_a_banner_not_a_card() {
        let mut st = with_orch();
        let mut o = ThreadScript::new("t-orch");
        o.at(80);
        let mut err = o.raise("oe1", Error);
        if let EventBody::AttentionRaised(p) = &mut err.body {
            p.summary = "orchestrator crashed (exit 1)".into();
            p.options = vec!["retry".into(), "dismiss".into()];
        }
        control(&mut st, &[err]);
        assert_eq!(st.order(), ["t-beta", "t-alpha", "t-gamma"]);
        assert_eq!(st.orch_banner().as_deref(), Some("t-orch"));
        assert_eq!(st.stack_nav()[0], "t-orch");
        st.set_focus(Some("t-beta".into()));
        st.handle_key(shift(KeyCode::BackTab));
        assert_eq!(st.focused(), Some("t-orch"));
        assert_eq!(
            run_row(&mut st, ActionKind::Dismiss),
            [Action::Dismiss {
                attn_id: "oe1".into(),
                revision: revision(&st, "oe1"),
            }]
        );
        // Its popup has no Talk row: the banner is never dived into.
        st.handle_key(key(KeyCode::Enter));
        let target = st.view.action.as_ref().expect("its popup").target.clone();
        assert!(st
            .action_rows(&target)
            .iter()
            .all(|r| r.kind != ActionKind::Talk));
        // Once resolved the banner is gone, and so are the selection and its popup.
        control(&mut st, &[o.resolve("oe1")]);
        assert_eq!(st.orch_banner(), None);
        assert_eq!(st.focused(), None);
        assert_eq!(st.view.action, None);
    }

    /// The revision of open item `attn_id` (known: folded with its control gseq).
    fn revision(st: &AppState, attn_id: &str) -> u64 {
        let r = st.attention.get(attn_id).unwrap().revision;
        assert!(r > 0, "{attn_id} has no revision");
        r
    }

    /// An option key answers the revision of the card on screen: after the item is raised
    /// again (asking something else) the key names the new revision, and an answer sent for
    /// the old one names the old one (the hub refuses it).
    #[test]
    fn an_answer_names_the_revision_on_screen() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        popup_open(&mut st);
        let shown = revision(&st, "pm1");
        let before = st.handle_key(key(KeyCode::Char('1')));
        let mut b = ThreadScript::new("t-beta");
        b.at(40);
        let mut again = b.raise("pm1", Permission);
        if let EventBody::AttentionRaised(p) = &mut again.body {
            p.summary = "Bash: rm -rf src".into();
        }
        control(&mut st, &[again]);
        let now = revision(&st, "pm1");
        assert!(now > shown);
        assert_eq!(st.attention.get("pm1").unwrap().first_revision, shown);
        assert_eq!(
            before,
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: shown,
                decision: "allow".into()
            }]
        );
        // Not answered before the new revision is on screen long enough to read: the popup
        // opened again shows it from now on.
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: now,
                decision: "allow".into()
            }]
        );
    }

    /// Dismiss, in the action popup of a hub-raised card (the review here) that has been on
    /// screen long enough, names its revision; a runtime's permission has no Dismiss row: it
    /// is answered, or its thread stopped.
    #[test]
    fn dismiss_is_for_hub_items_only() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        popup_open(&mut st);
        let target = st.view.action.as_ref().unwrap().target.clone();
        assert!(st
            .action_rows(&target)
            .iter()
            .all(|r| r.kind != ActionKind::Dismiss));
        st.handle_key(key(KeyCode::Esc));
        st.set_focus(Some("t-alpha".into()));
        // Just drawn: not yet.
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        let target = st.view.action.as_ref().unwrap().target.clone();
        assert!(st
            .action_rows(&target)
            .iter()
            .any(|r| r.kind == ActionKind::Dismiss));
        while st.view.action.as_ref().unwrap().sel != Some(ActionKind::Dismiss) {
            st.handle_key(key(KeyCode::Down));
        }
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(st.view.action.is_some(), "refused: the popup stays");
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Dismiss {
                attn_id: "rv1".into(),
                revision: revision(&st, "rv1"),
            }]
        );
        assert_eq!(st.view.action, None);
        assert_eq!(
            crate::action_request(&Action::Dismiss {
                attn_id: "rv1".into(),
                revision: 7,
            }),
            Some((
                "attention.dismiss",
                serde_json::json!({"attn_id": "rv1", "revision": 7})
            ))
        );
    }

    /// Draws a frame (160 x 40) and notes what it showed, as the IO loop does.
    pub(crate) fn drawn(st: &mut AppState) {
        draw(st, 160, 40);
        st.note_drawn(Instant::now());
    }

    /// Draws a frame and pretends what it showed has been there long enough to read.
    pub(crate) fn settled(st: &mut AppState) {
        drawn(st);
        if let Some(s) = st.view.shown.as_mut() {
            s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
        }
    }

    /// A replaced request at the keyboard: the runtime replaced its request (the adapter
    /// withdrew the old item and raised the new content under a new id) just before an option
    /// key meant for the old card. The key does not answer the new card: it has not been on
    /// screen long enough to be read. Once it has, the key answers it.
    #[test]
    fn a_key_never_answers_a_card_that_just_replaced_the_one_read() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        popup_open(&mut st);
        assert!(matches!(
            st.handle_key(key(KeyCode::Char('1'))).as_slice(),
            [Action::Respond { attn_id, .. }] if attn_id == "pm1"
        ));
        // The replacement arrives and is drawn; the user's key was meant for pm1.
        let mut b = ThreadScript::new("t-beta");
        b.at(40);
        let gone = b.event(EventBody::AttentionResolved(AttentionResolved {
            attn_id: "pm1".into(),
            by: ResolvedBy::Timeout,
            ..Default::default()
        }));
        let mut new = b.raise("pm1~1", Permission);
        if let EventBody::AttentionRaised(p) = &mut new.body {
            p.summary = "Bash: rm -rf precious".into();
        }
        control(&mut st, &[gone, new]);
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("the question just changed; read it and answer again")
        );
        // A card that changed after the last draw is not answered either.
        let mut again = b.raise("pm1~1", Permission);
        if let EventBody::AttentionRaised(p) = &mut again.body {
            p.summary = "Bash: rm -rf everything".into();
        }
        settled(&mut st);
        control(&mut st, &[again]);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        // Read (on screen long enough): answered, with the revision shown.
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('2'))),
            [Action::Respond {
                attn_id: "pm1~1".into(),
                revision: revision(&st, "pm1~1"),
                decision: "deny".into()
            }]
        );
    }

    /// `/orch` alone opens the orchestrator picker, the current runtime highlighted; its keys
    /// are its own (nothing typed meanwhile goes anywhere), and choosing asks to confirm the
    /// switch: `y` with the current one's summary, `s` without, any other key cancels. Esc
    /// closes it, asking nothing. `/orch <runtime> [model]` asks at once.
    #[test]
    fn orch_alone_picks_the_runtime_and_asks_to_confirm() {
        let mut st = with_orch();
        assert!(type_line(&mut st, "/orch").is_empty());
        assert!(st.picker_open());
        let p = st.view.picker.clone().unwrap();
        assert!(p.switch);
        assert_eq!(p.default, "claude-code");
        assert_eq!(p.runtimes[p.selected].0, "claude-code", "the current one");
        let s = crate::ui::picker_lines(&st)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            s.contains("Switch the orchestrator to") && s.contains("(current)"),
            "{s}"
        );
        st.handle_key(key(KeyCode::Char('x')));
        assert!(st.view.input.is_empty() && st.picker_open());
        st.handle_key(key(KeyCode::Down));
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(!st.picker_open());
        let prompt = st.view.confirm.as_ref().unwrap().prompt.clone();
        assert!(
            prompt.contains("to codex?")
                && prompt.contains("handoff")
                && prompt.contains("s: without"),
            "{prompt}"
        );
        assert_eq!(
            st.handle_key(key(KeyCode::Char('y'))),
            [Action::UseOrchestrator {
                runtime: "codex".into(),
                model: None,
                summary: true,
            }]
        );
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("handoff summary"));
        // `s`: without the summary.
        type_line(&mut st, "/orch");
        st.handle_key(key(KeyCode::Char('3')));
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            st.handle_key(key(KeyCode::Char('s'))),
            [Action::UseOrchestrator {
                runtime: "grok-build".into(),
                model: None,
                summary: false,
            }]
        );
        // Esc closes it, asking nothing; any other key cancels the question.
        type_line(&mut st, "/orch");
        st.handle_key(key(KeyCode::Esc));
        assert!(!st.picker_open() && st.view.confirm.is_none());
        type_line(&mut st, "/orch");
        st.handle_key(key(KeyCode::Enter));
        assert!(st.handle_key(key(KeyCode::Char('n'))).is_empty());
        assert_eq!(st.view.status.as_deref(), Some("cancelled"));
        // With a runtime named, it asks at once.
        assert!(type_line(&mut st, "/orch codex o5").is_empty());
        let prompt = st.view.confirm.as_ref().unwrap().prompt.clone();
        assert!(prompt.contains("codex (o5)"), "{prompt}");
    }

    /// Types `line` on a cleared input line and presses Enter.
    fn type_line(st: &mut AppState, line: &str) -> Vec<Action> {
        st.view.input.clear();
        for c in line.chars() {
            assert!(st.handle_key(key(KeyCode::Char(c))).is_empty());
        }
        st.handle_key(key(KeyCode::Enter))
    }

    #[test]
    fn clear_confirms_then_follows_a_fresh_orchestrator_and_keeps_worker_transcripts() {
        let mut st = with_orch();
        assert!(type_line(&mut st, "/clear").is_empty());
        assert!(st.view.confirm.is_some());
        assert!(st.handle_key(key(KeyCode::Char('n'))).is_empty());
        assert_eq!(st.main_thread().as_deref(), Some("t-orch"));
        assert!(type_line(&mut st, "/clear").is_empty());
        assert_eq!(
            st.handle_key(key(KeyCode::Char('y'))),
            [Action::ResetOrchestrator]
        );
        assert_eq!(
            crate::action_request(&Action::ResetOrchestrator),
            Some(("orchestrator.reset", serde_json::json!({})))
        );
        st.dive("t-alpha");
        assert_eq!(st.main_thread().as_deref(), Some("t-alpha"));
        st.view.scroll = 12;
        let attention = st.attention.len();
        let mut script = ThreadScript::new("fresh-orch");
        let mut ev = script.created("orchestrator");
        if let EventBody::ThreadCreated(p) = &mut ev.body {
            p.ext =
                serde_json::json!({"twr": {"role": "orchestrator", "conversation_reset": true}})
                    .as_object()
                    .cloned();
        }
        st.apply_control(&ev);
        st.apply_result(
            "orchestrator.reset",
            &serde_json::json!({"thread": "fresh-orch"}),
        );
        assert_eq!(st.main_thread().as_deref(), Some("fresh-orch"));
        assert!(st.transcript.as_ref().unwrap().entries.is_empty());
        assert_eq!(st.view.scroll, 0);
        assert_eq!(st.attention.len(), attention);
        assert!(st.transcripts.iter().any(|t| t.thread == "t-alpha"));
        assert!(!st.transcripts.iter().any(|t| t.thread == "t-orch"));
        assert_eq!(
            crate::slash::parse("/clear"),
            Ok(crate::slash::Slash::Clear)
        );
        assert!(crate::slash::parse("/clear alpha").is_err());
    }

    #[test]
    fn an_external_reset_keeps_a_worker_draft_and_unrelated_confirmation() {
        let mut st = with_orch();
        st.dive("t-alpha");
        assert!(type_line(&mut st, "/park alpha").is_empty());
        let confirmation = st.view.confirm.as_ref().unwrap().action.clone();
        st.view.input.insert("draft for alpha");
        let mut script = ThreadScript::new("fresh-orch");
        let mut ev = script.created("orchestrator");
        if let EventBody::ThreadCreated(p) = &mut ev.body {
            p.ext =
                serde_json::json!({"twr": {"role": "orchestrator", "conversation_reset": true}})
                    .as_object()
                    .cloned();
        }
        st.apply_control(&ev);
        assert_eq!(st.main_thread().as_deref(), Some("t-alpha"));
        assert_eq!(st.view.input.as_str(), "draft for alpha");
        assert_eq!(st.view.confirm.as_ref().unwrap().action, confirmation);
        st.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Send {
                thread: "t-alpha".into(),
                text: "draft for alpha".into()
            }]
        );
        st.back();
        assert_eq!(st.main_thread().as_deref(), Some("fresh-orch"));
        assert!(st.transcript.as_ref().unwrap().entries.is_empty());
        // A confirmation for the old orchestrator is withdrawn with an explanation.
        assert!(type_line(&mut st, "/orch codex").is_empty());
        ev.thread = "next-orch".into();
        st.apply_control(&ev);
        assert!(st.view.confirm.is_none());
        assert_eq!(
            st.view.status.as_deref(),
            Some("cancelled: the orchestrator was reset")
        );
    }

    #[test]
    fn replaying_a_past_reset_does_not_cancel_a_new_confirmation() {
        let mut st = with_orch();
        assert!(type_line(&mut st, "/orch codex").is_empty());
        st.begin_catch_up(u64::MAX);
        let mut ev = ThreadScript::new("fresh-orch").created("orchestrator");
        if let EventBody::ThreadCreated(p) = &mut ev.body {
            p.ext =
                serde_json::json!({"twr": {"role": "orchestrator", "conversation_reset": true}})
                    .as_object()
                    .cloned();
        }
        st.apply_control(&ev);
        assert!(st.view.confirm.is_some());
    }

    #[test]
    fn slash_commands_run_locally_and_never_as_messages() {
        let mut st = five_threads();
        st.set_focus(Some("t-gamma".into()));
        // /orch (and /orchestrator): the same confirmation as `o`.
        assert!(type_line(&mut st, "/orchestrator codex o5").is_empty());
        assert!(st.view.input.is_empty());
        assert_eq!(
            st.handle_key(key(KeyCode::Char('y'))),
            [Action::UseOrchestrator {
                runtime: "codex".into(),
                model: Some("o5".into()),
                summary: true,
            }]
        );
        assert!(type_line(&mut st, "/orch grok-build").is_empty());
        assert_eq!(
            st.handle_key(key(KeyCode::Char('S'))),
            [Action::UseOrchestrator {
                runtime: "grok-build".into(),
                model: None,
                summary: false,
            }]
        );
        // /new goes straight to the hub.
        assert_eq!(
            type_line(&mut st, "/new ~/src/api --runtime codex --name api2"),
            [Action::NewThread {
                project: "~/src/api".into(),
                runtime: Some("codex".into()),
                name: Some("api2".into()),
            }]
        );
        // /park and /stop ask first; the default is the focused thread.
        assert!(type_line(&mut st, "/park").is_empty());
        assert_eq!(
            st.view.confirm.as_ref().unwrap().prompt,
            "park gamma? y to confirm, any other key cancels"
        );
        assert_eq!(
            st.handle_key(key(KeyCode::Char('y'))),
            [Action::Park {
                thread: "t-gamma".into()
            }]
        );
        assert!(type_line(&mut st, "/stop alpha").is_empty());
        assert_eq!(
            st.view.confirm.as_ref().unwrap().prompt,
            "stop alpha? y to confirm, any other key cancels"
        );
        assert_eq!(
            st.handle_key(key(KeyCode::Char('y'))),
            [Action::Stop {
                thread: "t-alpha".into()
            }]
        );
        assert!(type_line(&mut st, "/stop alpha").is_empty());
        assert!(st.handle_key(key(KeyCode::Char('n'))).is_empty());
        assert_eq!(st.view.status.as_deref(), Some("cancelled"));
        // /resume runs at once, only on a parked thread.
        assert_eq!(
            type_line(&mut st, "/resume delta"),
            [Action::Resume {
                thread: "t-delta".into()
            }]
        );
        assert!(type_line(&mut st, "/resume").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("gamma is not parked"));
        assert_eq!(
            st.view.input, "/resume",
            "a failed command stays on the line"
        );
        // /attach shows the command to run elsewhere.
        assert!(type_line(&mut st, "/attach beta").is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("to attach, run in another terminal: twr attach beta")
        );
        // A thread that does not exist; an unknown command: errors, nothing sent.
        assert!(type_line(&mut st, "/park nope").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no thread nope"));
        assert!(st.view.confirm.is_none());
        assert!(type_line(&mut st, "/deploy now").is_empty());
        assert!(
            st.view
                .status
                .as_deref()
                .unwrap()
                .starts_with("unknown command /deploy"),
            "{:?}",
            st.view.status
        );
        assert_eq!(st.view.input, "/deploy now");
        // `//` sends the line with one `/`; `>` is no prefix any more, and plain text goes to
        // the orchestrator.
        draw(&mut st, 160, 60);
        let seen_gseq = st.last_gseq;
        assert_eq!(
            type_line(&mut st, "//tmp is full"),
            [Action::Say {
                text: "/tmp is full".into(),
                seen_gseq,
            }]
        );
        // `//>x` says `/>x`; it is not a `>` line.
        assert_eq!(
            type_line(&mut st, "//>foo"),
            [Action::Say {
                text: "/>foo".into(),
                seen_gseq,
            }]
        );
        assert_eq!(
            type_line(&mut st, ">/park is a word here"),
            [Action::Say {
                text: ">/park is a word here".into(),
                seen_gseq,
            }]
        );
        assert_eq!(
            type_line(&mut st, "deploy api"),
            [Action::Say {
                text: "deploy api".into(),
                seen_gseq,
            }]
        );
    }

    #[test]
    fn help_opens_and_any_key_closes_it() {
        let mut st = three_threads();
        assert!(type_line(&mut st, "/help").is_empty());
        assert!(st.view.help);
        assert!(st.view.input.is_empty());
        // The key that closes it does nothing else (not even quit, nor type).
        assert!(st.handle_key(key(KeyCode::Char('q'))).is_empty());
        assert!(!st.view.help);
        assert!(st.view.input.is_empty());
        // `?` on an empty line opens it; after text it is a character.
        assert!(st.handle_key(key(KeyCode::Char('?'))).is_empty());
        assert!(st.view.help);
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(!st.view.help);
        assert!(st.view.input.is_empty(), "Esc only closed the help");
        typed(&mut st, "why?");
        assert!(!st.view.help);
        assert_eq!(st.view.input.as_str(), "why?");
        st.view.input.clear();
        // `?` opens it on the decisions overlay too.
        type_line(&mut st, "/decisions");
        assert_eq!(st.view.screen, Screen::Decisions);
        assert!(st.handle_key(key(KeyCode::Char('?'))).is_empty());
        assert!(st.view.help);
        assert!(st.handle_key(key(KeyCode::Char('j'))).is_empty());
        assert!(!st.view.help);
        assert_eq!(st.view.screen, Screen::Decisions);
        // Ctrl-C still quits.
        st.view.help = true;
        assert_eq!(st.handle_key(ctrl('c')), [Action::Quit]);
    }

    #[test]
    fn tab_completes_a_slash_command_and_moves_focus_otherwise() {
        let mut st = three_threads();
        // The popup completes the selected row: a command that takes arguments gets a space.
        st.view.input = "/orc".into();
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.view.input, "/orch ");
        st.view.input = "/orch c".into();
        st.handle_key(key(KeyCode::Down));
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.view.input, "/orch codex ");
        // The hub's list replaces the built-in one.
        st.view.orch_runtimes = vec!["gemini".into()];
        st.view.input = "/orch g".into();
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.view.input, "/orch gemini ");
        st.view.input = "/stop be".into();
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.view.input, "/stop beta ");
        // With the popup closed (Esc), Tab completes the common prefix and lists the rest.
        st.view.input = "/o".into();
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.input, "/o", "Esc closed only the popup");
        assert!(st.popup().is_none());
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.view.input, "/o");
        assert_eq!(
            st.view.status.as_deref(),
            Some("/open  /orch  /orchestrator")
        );
        // Not a command: Tab moves focus as always.
        let before = st.focused().map(str::to_string);
        st.view.input = "hello".into();
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.view.input, "hello");
        assert_ne!(st.focused().map(str::to_string), before);
        // Shift-Tab completes nothing, the popup open or not: it selects.
        st.view.input = "/st".into();
        assert!(st.popup().is_some());
        let before = st.focused().map(str::to_string);
        st.handle_key(shift(KeyCode::BackTab));
        assert_eq!(st.view.input, "/st");
        assert_ne!(st.focused().map(str::to_string), before);
    }

    /// A digit in the action popup answers the selection's top item with that option; it
    /// closes the popup. An option there is not says so, the popup open.
    #[test]
    fn number_answers_top_attention_with_its_option() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        assert_eq!(
            answer_with(&mut st, 2),
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: revision(&st, "pm1"),
                decision: "deny".into()
            }]
        );
        assert_eq!(st.view.action, None);
        assert_eq!(
            answer_with(&mut st, 1),
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: revision(&st, "pm1"),
                decision: "allow".into()
            }]
        );
        popup_open(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('3'))).is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no option 3"));
        assert!(st.view.action.is_some());
        st.handle_key(key(KeyCode::Esc));

        st.set_focus(Some("t-alpha".into()));
        assert_eq!(
            answer_with(&mut st, 1),
            [Action::Respond {
                attn_id: "rv1".into(),
                revision: revision(&st, "rv1"),
                decision: "retry".into()
            }]
        );

        st.set_focus(Some("t-gamma".into()));
        popup_open(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert!(st.view.status.as_deref().unwrap().contains("nothing"));
    }

    /// Typing always edits the input line, a card selected or not: digits and `q` are text,
    /// never commands. Enter sends it.
    #[test]
    fn typing_is_always_text_and_enter_sends_it() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        for c in "q 1x".chars() {
            assert!(st.handle_key(key(KeyCode::Char(c))).is_empty());
        }
        st.handle_key(key(KeyCode::Backspace));
        assert_eq!(st.view.input, "q 1");
        // It carries the control position on screen: a reply answers only what was seen
        // (everything here, once drawn; [`AppState::seen_gseq`]).
        assert!(st.last_gseq > 0);
        draw(&mut st, 160, 40);
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Say {
                text: "q 1".into(),
                seen_gseq: st.last_gseq,
            }]
        );
        assert!(st.view.input.is_empty());
        // `>` is text like any other: to the orchestrator.
        for c in "> fix it".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Say {
                text: "> fix it".into(),
                seen_gseq: st.last_gseq,
            }]
        );
        // Blank input sends nothing.
        st.handle_key(key(KeyCode::Char(' ')));
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.handle_key(ctrl('c')), [Action::Quit]);
        // Esc keeps what is typed; `q` is a letter.
        st.handle_key(key(KeyCode::Esc));
        assert!(st.handle_key(key(KeyCode::Char('q'))).is_empty());
        assert_eq!(st.view.input, " q");
    }

    #[test]
    fn hub_questions_are_a_card_that_can_be_answered() {
        let mut st = three_threads();
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(30);
        let mut q = h.raise("hq1", Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = "Which thread should get: deploy?".into();
            p.options = vec!["alpha".into(), "beta".into(), "new_thread".into()];
        }
        control(&mut st, &[q]);
        assert!(st.hub_asks());
        assert_eq!(st.order(), [HUB_THREAD, "t-beta", "t-alpha", "t-gamma"]);
        // Nothing gets selected; the hub card is Tab away.
        assert_eq!(st.focused(), None);
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some(HUB_THREAD));
        assert_eq!(
            answer_with(&mut st, 2),
            [Action::Respond {
                attn_id: "hq1".into(),
                revision: revision(&st, "hq1"),
                decision: "beta".into()
            }]
        );
        // Its popup neither stops nor parks the hub: it has its options, and Dismiss.
        popup_open(&mut st);
        let target = st.view.action.as_ref().unwrap().target.clone();
        let kinds: Vec<ActionKind> = st.action_rows(&target).iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                ActionKind::Answer(1),
                ActionKind::Answer(2),
                ActionKind::Answer(3),
                ActionKind::Dismiss
            ]
        );
        // `/stop` without a thread names none.
        st.handle_key(key(KeyCode::Esc));
        assert!(type_line(&mut st, "/stop").is_empty());
        assert!(st.view.confirm.is_none());
        assert_eq!(st.view.status.as_deref(), Some("no thread selected"));
        st.view.input.clear();
        for c in "hi".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Say {
                text: "hi".into(),
                seen_gseq: st.last_gseq,
            }]
        );
        // Once answered the card goes away, and so does the selection.
        control(&mut st, &[h.resolve("hq1")]);
        assert!(!st.hub_asks());
        assert_eq!(st.order(), ["t-beta", "t-alpha", "t-gamma"]);
        assert_eq!(st.focused(), None);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut st = three_threads();
        let mut k = key(KeyCode::Char('q'));
        k.kind = KeyEventKind::Release;
        assert!(st.handle_key(k).is_empty());
    }

    #[test]
    fn stop_and_scroll() {
        let mut st = three_threads();
        st.handle_key(key(KeyCode::PageUp));
        st.handle_key(key(KeyCode::PageUp));
        assert_eq!(st.view.scroll, 2 * SCROLL_STEP);
        st.handle_key(key(KeyCode::PageDown));
        st.handle_key(key(KeyCode::PageDown));
        st.handle_key(key(KeyCode::PageDown));
        assert_eq!(st.view.scroll, 0);
        st.set_focus(Some("t-alpha".into()));
        assert_eq!(
            run_row(&mut st, ActionKind::Stop),
            [Action::Stop {
                thread: "t-alpha".into()
            }]
        );
        assert_eq!(st.view.action, None);
    }

    #[test]
    fn pin_does_not_reorder_across_reasons_but_is_kept() {
        let mut st = three_threads();
        let before = st.order();
        // Pin the review thread: the permission thread stays leftmost.
        st.set_focus(Some("t-alpha".into()));
        assert!(run_row(&mut st, ActionKind::Pin).is_empty());
        assert!(st.view.pins.contains("t-alpha"));
        assert_eq!(st.view.status.as_deref(), Some("pinned alpha"));
        assert_eq!(st.order(), before);
        // The pin survives focus changes and further events.
        st.handle_key(key(KeyCode::Tab));
        let mut a = ThreadScript::new("t-alpha");
        a.at(100);
        control(&mut st, &[a.git("main", 1)]);
        assert!(st.view.pins.contains("t-alpha"));
        // Toggling again removes it; `/pin <thread>` toggles too.
        st.set_focus(Some("t-alpha".into()));
        assert!(run_row(&mut st, ActionKind::Pin).is_empty());
        assert!(st.view.pins.is_empty());
        type_line(&mut st, "/pin gamma");
        assert!(st.view.pins.contains("t-gamma"));
        type_line(&mut st, "/pin gamma");
        assert!(st.view.pins.is_empty());
        assert!(type_line(&mut st, "/pin orch").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no thread orch"));
    }

    /// One `hub.info` box: online unless `state` says otherwise.
    pub(crate) fn info_box(name: &str, state: &str) -> Value {
        serde_json::json!({
            "name": name, "transport": "ssh", "state": state,
            "online": state == "online" || state == "switching",
            "version": "0.2.0", "refused": null, "last_error": null, "notes": [],
            "os": "Linux", "arch": "x86_64", "build": "aaaa", "running_build": "aaaa",
            "switch_pending": false, "agents": [],
        })
    }

    /// What a box's token says, by its `hub.info` entry: `switching` first, then `update
    /// pending`, then why it is not online; a protocol mismatch only from a refusal or error
    /// that names a protocol, never from another build or version alone.
    #[test]
    fn a_box_word_follows_its_hub_info_entry() {
        let word = |b: Value| BoxStatus::from_info(&b).word();
        let with = |mut b: Value, k: &str, v: Value| {
            b[k] = v;
            b
        };
        assert_eq!(word(info_box("gpu", "online")), None);
        // A box of another OS and CPU on another build and version, online: plain.
        let cross = with(
            with(info_box("mac", "online"), "running_build", "bbbb".into()),
            "version",
            "0.1.9".into(),
        );
        let cross = with(cross, "os", "Darwin".into());
        assert_eq!(word(cross), None);
        // A token box (no SSH fields) online, and offline.
        let token = serde_json::json!({
            "name": "pi", "transport": "token", "state": "online", "online": true,
            "version": "0.2.0", "refused": null, "last_error": null, "notes": null,
            "switch_pending": null, "agents": [],
        });
        assert_eq!(word(token.clone()), None);
        let token = with(
            with(token, "state", "offline".into()),
            "online",
            false.into(),
        );
        assert_eq!(word(token.clone()), Some(BoxWord::Offline));
        // Refused for its protocol: a mismatch.
        let refused = serde_json::json!({"at": "2026-10-17T10:00:00Z", "reason":
            "a runner speaking protocol 6 was refused (the hub's is 7): update twr on box `pi`"});
        assert_eq!(
            word(with(token.clone(), "refused", refused)),
            Some(BoxWord::Mismatch)
        );
        // Refused for its token: offline, not a mismatch.
        let bad_token = serde_json::json!({"at": "2026-10-17T10:00:00Z", "reason":
            "a runner was refused: its proof does not match this box's token"});
        assert_eq!(
            word(with(token, "refused", bad_token)),
            Some(BoxWord::Offline)
        );
        // An online box being switched; one with a switch pending.
        assert_eq!(word(info_box("gpu", "switching")), Some(BoxWord::Switching));
        let pending = with(info_box("gpu", "online"), "switch_pending", true.into());
        assert_eq!(word(pending.clone()), Some(BoxWord::UpdatePending));
        // Switching wins over pending, pending over offline.
        let both = with(info_box("gpu", "switching"), "switch_pending", true.into());
        assert_eq!(word(both), Some(BoxWord::Switching));
        let away = with(
            with(pending, "state", "offline".into()),
            "online",
            false.into(),
        );
        assert_eq!(word(away), Some(BoxWord::UpdatePending));
        for (state, want) in [
            ("needs_install", BoxWord::NeedsInstall),
            ("error", BoxWord::Error),
            ("offline", BoxWord::Offline),
            ("connecting", BoxWord::Reconnecting),
        ] {
            assert_eq!(word(info_box("gpu", state)), Some(want), "{state}");
        }
        // A cross-target box whose build speaks another protocol: `needs install` (what to do)
        // wins over the mismatch that caused it.
        let old = with(
            info_box("mac", "needs_install"),
            "last_error",
            "box mac runs a twr build that speaks protocol 6, the hub 7: install this hub's \
             build there: `twr box install mac`"
                .into(),
        );
        assert_eq!(word(old), Some(BoxWord::NeedsInstall));
        // An error that only mentions a protocol in passing is not one.
        let ssh = with(
            info_box("gpu", "offline"),
            "last_error",
            "ssh: the protocol banner was not read".into(),
        );
        assert_eq!(word(ssh), Some(BoxWord::Offline));
    }

    /// A thread on a box the hub's boxes do not name (removed since) is `not configured` once
    /// a `hub.info` answer listed them, and says nothing before (no answer yet, or answers
    /// without boxes); a box added back, or whose runner connects, is itself again. A local
    /// thread never has a word.
    #[test]
    fn a_box_missing_from_the_hub_is_not_configured() {
        use twr_core::event::RunnerPayload;
        let mut st = AppState::new();
        let mut job = ThreadScript::new("t-job");
        let mut created = job.created("job");
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.box_name = "gpu".into();
        }
        let mut loc = ThreadScript::new("t-loc");
        let mut local = loc.created("loc");
        if let EventBody::ThreadCreated(p) = &mut local.body {
            p.box_name = "local".into();
        }
        control(&mut st, &[created, local]);
        let word = |st: &AppState, id: &str| st.box_word(st.registry.get(id).unwrap());
        assert_eq!(word(&st, "t-job"), None);
        st.apply_result("hub.info", &Value::Null);
        st.apply_result("hub.info", &serde_json::json!({ "disk": {} }));
        assert!(!st.boxes_known);
        assert_eq!(word(&st, "t-job"), None);
        st.apply_result(
            "hub.info",
            &serde_json::json!({ "boxes": [info_box("pi", "online")] }),
        );
        assert!(st.boxes_known);
        assert_eq!(word(&st, "t-job"), Some(BoxWord::NotConfigured));
        assert_eq!(word(&st, "t-loc"), None);
        // A failed poll after it changes nothing; an empty list leaves it unconfigured.
        st.apply_result("hub.info", &Value::Null);
        assert_eq!(word(&st, "t-job"), Some(BoxWord::NotConfigured));
        st.apply_result("hub.info", &serde_json::json!({ "boxes": [] }));
        assert_eq!(word(&st, "t-job"), Some(BoxWord::NotConfigured));
        assert_eq!(word(&st, "t-loc"), None);
        // Added back.
        st.apply_result(
            "hub.info",
            &serde_json::json!({ "boxes": [info_box("gpu", "offline")] }),
        );
        assert_eq!(word(&st, "t-job"), Some(BoxWord::Offline));
        st.apply_result("hub.info", &serde_json::json!({ "boxes": [] }));
        assert_eq!(word(&st, "t-job"), Some(BoxWord::NotConfigured));
        // Its runner connecting before the next poll names it.
        let mut hub = ThreadScript::new(twr_core::event::HUB_THREAD);
        let up = hub.event(EventBody::RunnerConnected(RunnerPayload {
            box_name: "gpu".into(),
            ..Default::default()
        }));
        control(&mut st, &[up]);
        assert_eq!(word(&st, "t-job"), None);
        assert_eq!(BoxWord::NotConfigured.text(), "not configured");
    }

    /// `hub.info` polls replace the boxes (a failed one keeps them); `runner.connected` and
    /// `runner.disconnected` change a box at once between polls, but not during the replay.
    #[test]
    fn runner_events_change_a_box_between_polls() {
        use twr_core::event::RunnerPayload;
        let mut st = AppState::new();
        st.apply_result(
            "hub.info",
            &serde_json::json!({"boxes": [info_box("gpu", "offline")], "disk": {}}),
        );
        assert_eq!(st.boxes["gpu"].word(), Some(BoxWord::Offline));
        st.apply_result("hub.info", &Value::Null);
        assert_eq!(st.boxes["gpu"].word(), Some(BoxWord::Offline));
        let mut hub = ThreadScript::new(twr_core::event::HUB_THREAD);
        let runner = |name: &str| RunnerPayload {
            box_name: name.into(),
            version: Some("0.2.1".into()),
            ..Default::default()
        };
        let up = hub.event(EventBody::RunnerConnected(runner("gpu")));
        let down = hub.event(EventBody::RunnerDisconnected(runner("gpu")));
        control(&mut st, std::slice::from_ref(&up));
        assert_eq!(st.boxes["gpu"].word(), None);
        assert_eq!(st.boxes["gpu"].version.as_deref(), Some("0.2.1"));
        control(&mut st, std::slice::from_ref(&down));
        assert_eq!(st.boxes["gpu"].word(), Some(BoxWord::Offline));
        // A box being switched stays so while its runner is away; back, it is on its build.
        let mut switching = info_box("gpu", "switching");
        switching["switch_pending"] = true.into();
        assert!(st.send_info());
        st.apply_result("hub.info", &serde_json::json!({ "boxes": [switching] }));
        control(&mut st, std::slice::from_ref(&down));
        assert_eq!(st.boxes["gpu"].word(), Some(BoxWord::Switching));
        control(&mut st, std::slice::from_ref(&up));
        assert_eq!(st.boxes["gpu"].word(), None);
        // A box live-added since the last poll.
        let new = hub.event(EventBody::RunnerConnected(runner("pi")));
        control(&mut st, &[new]);
        assert_eq!(st.boxes["pi"].word(), None);
        // The replay's connections are past ones.
        let mut st = AppState::new();
        st.apply_boxes(&serde_json::json!({"boxes": [info_box("gpu", "online")]}));
        st.begin_catch_up(10);
        control(&mut st, &[down]);
        assert_eq!(st.boxes["gpu"].word(), None);
        // A poll says which boxes there are: one removed goes.
        assert!(st.send_info());
        st.apply_result("hub.info", &serde_json::json!({"boxes": []}));
        assert!(st.boxes.is_empty());
    }

    /// A `hub.info` answer does not undo a runner event newer than its request: a poll sent
    /// before a disconnect, answered after it, leaves the box offline (and one it no longer
    /// names but the event did, there); a poll sent after the event applies; an answer that
    /// agrees on the box being online or not applies whenever it was sent.
    #[test]
    fn an_older_poll_does_not_undo_a_runner_event() {
        use twr_core::event::RunnerPayload;
        let mut st = AppState::new();
        let mut hub = ThreadScript::new(twr_core::event::HUB_THREAD);
        let runner = |name: &str| RunnerPayload {
            box_name: name.into(),
            ..Default::default()
        };
        let online = || serde_json::json!({"boxes": [info_box("gpu", "online")]});
        assert!(st.send_info());
        st.apply_result("hub.info", &online());
        assert_eq!(st.boxes["gpu"].word(), None);
        // Sent while online, answered after the box left.
        assert!(st.send_info());
        assert!(!st.send_info(), "one poll at a time");
        let down = hub.event(EventBody::RunnerDisconnected(runner("gpu")));
        control(&mut st, &[down]);
        st.apply_result("hub.info", &online());
        assert_eq!(st.boxes["gpu"].word(), Some(BoxWord::Offline));
        // A box added after the poll was sent, which its answer does not name yet, stays.
        assert!(st.send_info());
        let pi_up = hub.event(EventBody::RunnerConnected(runner("pi")));
        control(&mut st, &[pi_up]);
        st.apply_result(
            "hub.info",
            &serde_json::json!({"boxes": [info_box("gpu", "offline")]}),
        );
        assert_eq!(st.boxes["pi"].word(), None);
        assert_eq!(st.boxes["gpu"].word(), Some(BoxWord::Offline));
        // An older answer that agrees still applies (what it adds: its state, version...).
        assert!(st.send_info());
        let up = hub.event(EventBody::RunnerConnected(runner("gpu")));
        control(&mut st, &[up]);
        let mut newer = info_box("gpu", "online");
        newer["version"] = "0.2.9".into();
        st.apply_result("hub.info", &serde_json::json!({ "boxes": [newer] }));
        assert_eq!(st.boxes["gpu"].version.as_deref(), Some("0.2.9"));
        // A poll sent after the event applies, whatever it says.
        assert!(st.send_info());
        st.apply_result(
            "hub.info",
            &serde_json::json!({"boxes": [info_box("gpu", "offline")]}),
        );
        assert_eq!(st.boxes["gpu"].word(), Some(BoxWord::Offline));
        assert!(!st.boxes.contains_key("pi"));
    }

    #[test]
    fn control_replay_is_deduplicated_by_gseq() {
        let mut st = AppState::new();
        let mut s = ThreadScript::new("t1");
        let ev = s.created("one");
        let item = StreamItem {
            stream: StreamKind::Control,
            gseq: Some(1),
            event: ev,
        };
        st.apply_event(&item);
        let mut raise = item.clone();
        raise.gseq = Some(2);
        raise.event = s.raise("a1", Question);
        st.apply_event(&raise);
        let mut resolve = item.clone();
        resolve.gseq = Some(3);
        resolve.event = s.resolve("a1");
        st.apply_event(&resolve);
        // A replayed raise with an old gseq must not resurrect the item.
        st.apply_event(&raise);
        assert!(st.attention.is_empty());
        assert_eq!(st.last_gseq, 3);
    }

    #[test]
    fn transcript_coalesces_deltas_and_tools() {
        let mut s = ThreadScript::new("t1");
        let mut tr = Transcript::new("t1");
        let delta = |s: &mut ThreadScript, t: &str| {
            s.event(EventBody::AgentText(AgentText {
                text: t.into(),
                delta: true,
                ..Default::default()
            }))
        };
        let evs = vec![
            s.created("one"),
            s.turn_started("say hi"),
            delta(&mut s, "hel"),
            delta(&mut s, "lo"),
            s.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: "c1".into(),
                tool: "Bash".into(),
                input_summary: "ls".into(),
                ..Default::default()
            })),
            s.event(EventBody::AgentToolResult(AgentToolResult {
                call_id: "c1".into(),
                ok: true,
                ..Default::default()
            })),
            s.text("hello world"),
            s.turn_ended(TurnResult::Completed),
        ];
        for ev in &evs {
            tr.apply(ev);
        }
        // Replays are ignored.
        tr.apply(&evs[2]);
        assert_eq!(
            tr.entries,
            vec![
                Entry::TurnStart {
                    seq: evs[1].seq,
                    turn: Some(1),
                    from: TurnFrom::User,
                    prompt: "say hi".into(),
                    notice: false,
                    withdrawn: false,
                },
                Entry::Text {
                    seq: evs[2].seq,
                    text: "hello world".into(),
                    open: false
                },
                Entry::Tool {
                    call_id: "c1".into(),
                    tool: "Bash".into(),
                    summary: "ls".into(),
                    ok: Some(true),
                    thread: None
                },
                Entry::TurnEnd {
                    result: TurnResult::Completed,
                    duration_ms: None
                },
            ]
        );
    }

    /// [`three_threads`] plus `delta` (review item, then parked) and `eps` (done and reported,
    /// with a git snapshot). Stack: beta, alpha, delta, gamma, eps; grid: delta moves last.
    pub(crate) fn five_threads() -> AppState {
        use twr_core::event::{GitSnapshot, Grade};
        let mut st = three_threads();
        let mut d = ThreadScript::new("t-delta");
        let mut e = ThreadScript::new("t-eps");
        d.at(5);
        let mut evs = vec![d.created("delta"), d.raise("rv2", Review), d.parked()];
        e.at(30);
        evs.extend([
            e.created("eps"),
            e.turn_started("docs"),
            e.turn_ended(TurnResult::Completed),
        ]);
        let mut report = e.report(Some(1), Grade::Success);
        if let EventBody::ReportReady(p) = &mut report.body {
            p.summary = "Rewrote the README quick start and the architecture section. \
                         Links checked. Jump in with `twr attach eps` to review the prose."
                .into();
        }
        evs.push(report);
        evs.push(e.event(EventBody::GitSnapshot(GitSnapshot {
            branch: Some("twr/eps".into()),
            dirty_files: 3,
            ahead: 2,
            behind: 1,
            ..Default::default()
        })));
        control(&mut st, &evs);
        st
    }

    fn grid(width: u16) -> AppState {
        let mut st = five_threads();
        st.view.width = width;
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        st
    }

    #[test]
    fn grid_order_moves_parked_threads_last() {
        let st = five_threads();
        assert_eq!(
            st.order(),
            ["t-beta", "t-alpha", "t-delta", "t-gamma", "t-eps"]
        );
        assert_eq!(
            st.grid_order(),
            ["t-beta", "t-alpha", "t-gamma", "t-eps", "t-delta"]
        );
        // Tab follows the order of the view on screen.
        let mut st = grid(160);
        st.set_focus(Some("t-eps".into()));
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-delta"));
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-beta"));
    }

    #[test]
    fn grid_vertical_steps() {
        // 5 tiles, 2 per row: rows [0 1] [2 3] [4].
        assert_eq!(grid_step_vertical(0, 5, 2, true), 2);
        assert_eq!(
            grid_step_vertical(3, 5, 2, true),
            4,
            "into the short last row"
        );
        assert_eq!(grid_step_vertical(4, 5, 2, true), 0, "wraps to the top");
        assert_eq!(
            grid_step_vertical(1, 5, 2, false),
            3,
            "wraps past the short row"
        );
        assert_eq!(grid_step_vertical(0, 5, 2, false), 4);
        assert_eq!(grid_step_vertical(2, 5, 2, false), 0);
        // One row: up and down stay put.
        assert_eq!(grid_step_vertical(1, 3, 4, true), 1);
        assert_eq!(grid_step_vertical(1, 3, 4, false), 1);
        assert_eq!(grid_step_vertical(0, 0, 4, true), 0);
        assert_eq!(grid_columns(100), 2);
        assert_eq!(grid_columns(160), 4);
        assert_eq!(grid_columns(180), 4);
        assert_eq!(grid_columns(0), 1);
    }

    /// In the grid the Shift-arrows move between tiles in two dimensions (none selected:
    /// Shift-↓ the first, Shift-↑ the last); plain arrows are the input line's.
    #[test]
    fn grid_shift_arrows_move_in_two_dimensions() {
        // 100 columns: 2 per row. Grid: [beta alpha] [gamma eps] [delta].
        let mut st = grid(100);
        st.handle_key(shift(KeyCode::Down));
        assert_eq!(st.focused(), Some("t-beta"));
        let mut path = Vec::new();
        for code in [
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Left,
            KeyCode::Up,
        ] {
            st.handle_key(shift(code));
            path.push(st.focused().unwrap().to_string());
        }
        assert_eq!(
            path,
            ["t-gamma", "t-eps", "t-delta", "t-beta", "t-delta", "t-eps", "t-alpha"]
        );
        // Right on the last tile wraps to the first.
        st.set_focus(Some("t-delta".into()));
        st.handle_key(shift(KeyCode::Right));
        assert_eq!(st.focused(), Some("t-beta"));
        st.handle_key(key(KeyCode::Down));
        st.handle_key(key(KeyCode::Right));
        assert_eq!(st.focused(), Some("t-beta"), "plain arrows move no tile");
        st.set_focus(None);
        st.handle_key(shift(KeyCode::Up));
        assert_eq!(st.focused(), Some("t-delta"));
        // In the stack view Shift-↑/↓ move along the cards (a column when wide).
        st.set_focus(Some("t-beta".into()));
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Stack);
        st.handle_key(shift(KeyCode::Down));
        assert_eq!(st.focused(), Some("t-alpha"));
        st.handle_key(shift(KeyCode::Up));
        assert_eq!(st.focused(), Some("t-beta"));
    }

    /// Enter on a tile opens its popup (the stack view's, over the grid); Enter again dives
    /// in, in the stack view. Letters type, whatever is selected.
    #[test]
    fn enter_enter_on_a_tile_dives_into_it() {
        let mut st = grid(180);
        st.set_focus(Some("t-alpha".into()));
        st.handle_key(shift(KeyCode::Right));
        assert_eq!(st.focused(), Some("t-gamma"));
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(st.view.action.is_some());
        assert_eq!(st.view.screen, Screen::Grid);
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.screen, Screen::Stack);
        assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
        assert_eq!(st.transcript.as_ref().unwrap().thread, "t-gamma");
        // `g` is text.
        st.handle_key(key(KeyCode::Char('g')));
        assert_eq!(st.view.screen, Screen::Stack);
        assert_eq!(st.view.input, "g");
        // Esc returns to the orchestrator conversation in the stack view.
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (st.view.dive.as_deref(), st.view.screen),
            (None, Screen::Stack)
        );
        // The grid has no orchestrator tile.
        let mut st = grid(180);
        let mut o = ThreadScript::new("t-orch");
        o.at(70);
        let evs = orch_events(&mut o, "t-orch");
        control(&mut st, &evs);
        assert!(!st.grid_order().contains(&"t-orch".to_string()));
        assert_eq!(st.grid_order().len(), 5);
    }

    /// Park and Resume (the action popup's rows, `/park`) ask `y` first; the popup offers
    /// the one that applies.
    #[test]
    fn park_and_resume_need_a_one_key_confirmation() {
        let mut st = five_threads();
        st.set_focus(Some("t-gamma".into()));
        // Park asks first; nothing is sent yet.
        assert!(run_row(&mut st, ActionKind::Park).is_empty());
        assert_eq!(st.view.action, None);
        let c = st.view.confirm.clone().unwrap();
        assert_eq!(c.prompt, "park gamma? y to confirm, any other key cancels");
        // Any other key cancels (and is not acted on).
        assert!(st.handle_key(key(KeyCode::Char('q'))).is_empty());
        assert!(st.view.confirm.is_none());
        assert_eq!(st.view.status.as_deref(), Some("cancelled"));
        // `y` sends it.
        run_row(&mut st, ActionKind::Park);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('y'))),
            [Action::Park {
                thread: "t-gamma".into()
            }]
        );
        assert!(st.view.confirm.is_none());
        assert!(st.view.status.as_deref().unwrap().starts_with("park gamma"));
        // A thread that is not parked has no Resume row; `/resume` is refused locally.
        popup_open(&mut st);
        let target = st.view.action.as_ref().unwrap().target.clone();
        assert!(st
            .action_rows(&target)
            .iter()
            .all(|r| r.kind != ActionKind::Resume));
        st.handle_key(key(KeyCode::Esc));
        assert!(type_line(&mut st, "/resume").is_empty());
        assert!(st.view.confirm.is_none());
        assert_eq!(st.view.status.as_deref(), Some("gamma is not parked"));
        st.view.input.clear();
        // The parked thread resumes; parking it again is refused.
        st.set_focus(Some("t-delta".into()));
        assert!(type_line(&mut st, "/park").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("delta is already parked"));
        st.view.input.clear();
        run_row(&mut st, ActionKind::Resume);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('Y'))),
            [Action::Resume {
                thread: "t-delta".into()
            }]
        );
        // Esc cancels, and does nothing else; Ctrl-C still quits.
        run_row(&mut st, ActionKind::Resume);
        st.handle_key(key(KeyCode::Esc));
        assert!(st.view.confirm.is_none());
        assert_eq!(st.focused(), Some("t-delta"), "the Esc only cancelled");
        run_row(&mut st, ActionKind::Resume);
        assert_eq!(st.handle_key(ctrl('c')), [Action::Quit]);
        // Typed, P is a letter.
        let mut st = five_threads();
        st.set_focus(Some("t-gamma".into()));
        st.handle_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
        assert!(st.view.confirm.is_none());
        assert_eq!(st.view.input, "P");
    }

    /// A `decision.made` event on `thread` (turn `turn`) by `script`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decision(
        script: &mut ThreadScript,
        turn: Option<u64>,
        site: DecisionSite,
        provider: DecisionProvider,
        options: &[&str],
        chosen: &str,
        confidence: f64,
        latency_ms: u64,
    ) -> Event {
        let mut ev = script.event(EventBody::DecisionMade(twr_core::event::DecisionMade {
            site,
            provider,
            options: options.iter().map(|o| o.to_string()).collect(),
            chosen: chosen.into(),
            confidence,
            latency_ms,
            ..Default::default()
        }));
        ev.turn = turn;
        ev
    }

    /// [`five_threads`] plus six decisions (oldest first):
    /// 1. hub route by code → alpha
    /// 2. hub route by orchestrator → orchestrator (1ms)
    /// 3. hub route by user → gamma
    /// 4. beta grade by code → success at 0.60 (the default), turn 1
    /// 5. alpha block_kind by code → transient, turn 1
    /// 6. alpha block_kind by code → user_blocked (3ms), turn 2
    pub(crate) fn with_decisions() -> AppState {
        use DecisionProvider as P;
        use DecisionSite as S;
        let mut st = five_threads();
        let mut h = ThreadScript::new(HUB_THREAD);
        let mut a = ThreadScript::new("t-alpha");
        let mut b = ThreadScript::new("t-beta");
        h.at(3600);
        let route = ["alpha", "beta", "gamma", "new thread"];
        let mut evs = vec![
            decision(&mut h, None, S::Route, P::Code, &route, "alpha", 1.0, 0),
            decision(
                &mut h,
                None,
                S::Route,
                P::Orchestrator,
                &route,
                "orchestrator",
                1.0,
                1,
            ),
            decision(&mut h, None, S::Route, P::User, &route, "gamma", 1.0, 0),
        ];
        b.at(3700);
        let grades = ["success", "partial", "gave_up", "needs_verification"];
        evs.push(decision(
            &mut b,
            Some(1),
            S::Grade,
            P::Code,
            &grades,
            "success",
            0.6,
            0,
        ));
        a.at(3800);
        let kinds = ["transient", "user_blocked"];
        evs.push(decision(
            &mut a,
            Some(1),
            S::BlockKind,
            P::Code,
            &kinds,
            "transient",
            1.0,
            0,
        ));
        evs.push(decision(
            &mut a,
            Some(2),
            S::BlockKind,
            P::Code,
            &kinds,
            "user_blocked",
            1.0,
            3,
        ));
        control(&mut st, &evs);
        st
    }

    /// Opens the decisions overlay (`/decisions`).
    fn open_decisions(st: &mut AppState) {
        type_line(st, "/decisions");
        assert_eq!(st.view.screen, Screen::Decisions);
    }

    fn chosen(st: &AppState) -> Vec<String> {
        st.decision_rows()
            .iter()
            .map(|r| r.chosen.clone())
            .collect()
    }

    #[test]
    fn decisions_are_kept_newest_first() {
        let st = with_decisions();
        assert_eq!(
            chosen(&st),
            [
                "user_blocked",
                "transient",
                "success",
                "gamma",
                "orchestrator",
                "alpha"
            ]
        );
        let rows = st.decision_rows();
        assert_eq!(rows[0].turn, Some(2));
        assert_eq!(rows[0].thread, "t-alpha");
        assert_eq!(rows[5].thread, HUB_THREAD);
        assert_eq!(rows[5].time, "01:00:00");
        // Other kinds are not decisions; the registry still sees every event.
        assert_eq!(st.decisions.len(), 6);
        assert_eq!(st.registry.len(), 5);

        let s = st.decisions.summary();
        // code, orchestrator, user
        assert_eq!(s.by_provider, [4, 1, 1]);
        // Route decisions: code → alpha, the orchestrator, user → gamma; the last two are
        // fallbacks.
        assert_eq!((s.route_fallback, s.route_settled), (2, 3));
    }

    #[test]
    fn decision_filters_cycle_through_sites_and_providers() {
        let mut st = with_decisions();
        open_decisions(&mut st);
        let mut sites = Vec::new();
        for _ in 0..4 {
            st.handle_key(key(KeyCode::Char('f')));
            sites.push((st.view.decision_filter.site, chosen(&st).len()));
        }
        assert_eq!(
            sites,
            [
                (Some(DecisionSite::Route), 3),
                (Some(DecisionSite::Grade), 1),
                (Some(DecisionSite::BlockKind), 2),
                (None, 6),
            ]
        );
        let mut providers = Vec::new();
        for _ in 0..4 {
            st.handle_key(key(KeyCode::Char('v')));
            providers.push((st.view.decision_filter.provider, chosen(&st)));
        }
        assert_eq!(
            providers,
            [
                (
                    Some(DecisionProvider::Code),
                    vec![
                        "user_blocked".to_string(),
                        "transient".into(),
                        "success".into(),
                        "alpha".into()
                    ]
                ),
                (
                    Some(DecisionProvider::Orchestrator),
                    vec!["orchestrator".into()]
                ),
                (Some(DecisionProvider::User), vec!["gamma".into()]),
                (None, chosen(&st)),
            ]
        );
        // Both filters at once: code route decisions.
        st.handle_key(key(KeyCode::Char('f')));
        st.handle_key(key(KeyCode::Char('v')));
        assert_eq!(chosen(&st), ["alpha"]);
        // A filter change resets the selection.
        st.view.decision_sel = 3;
        st.handle_key(key(KeyCode::Char('f')));
        assert_eq!(st.view.decision_sel, 0);
    }

    #[test]
    fn enter_on_a_decision_opens_its_thread_in_the_stack_view() {
        let mut st = with_decisions();
        st.set_focus(Some("t-gamma".into()));
        open_decisions(&mut st);
        // Row 1 (transient, alpha).
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.decision_selected(), 1);
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.screen, Screen::Stack);
        assert_eq!(st.view.dive.as_deref(), Some("t-alpha"));
        assert_eq!(st.transcript.as_ref().unwrap().thread, "t-alpha");
        assert_eq!(st.view.focus_before_dive.as_deref(), Some("t-gamma"));

        // A hub decision has no thread: stay on the table and say so.
        open_decisions(&mut st);
        st.handle_key(key(KeyCode::End));
        assert_eq!(st.decision_selected(), 5);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.screen, Screen::Decisions);
        assert_eq!(
            st.view.dive.as_deref(),
            Some("t-alpha"),
            "the dive stays under it"
        );
        assert_eq!(
            st.view.status.as_deref(),
            Some("hub decision: no thread to open")
        );
        // Selection is clamped to the table.
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.decision_selected(), 5);
        st.handle_key(key(KeyCode::PageUp));
        assert_eq!(st.decision_selected(), 0);
        st.handle_key(key(KeyCode::PageDown));
        assert_eq!(st.decision_selected(), 5);
        st.handle_key(key(KeyCode::Home));
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.decision_selected(), 0);

        // An empty table: Enter only reports it.
        let mut empty = five_threads();
        open_decisions(&mut empty);
        empty.handle_key(key(KeyCode::Enter));
        assert_eq!(empty.view.screen, Screen::Decisions);
        assert_eq!(empty.view.status.as_deref(), Some("no decision selected"));
    }

    /// The decisions overlay keeps its own keys: letters and digits do nothing there but its
    /// own (no input box to type into, a paste neither), Ctrl-G does not switch views under
    /// it; Esc closes it, back to the screen it came from, with that screen's dive and
    /// selection as they were. Out of it, its letters type.
    #[test]
    fn the_decisions_overlay_closes_back_to_the_screen_it_came_from() {
        let mut st = with_decisions();
        st.handle_key(ctrl('g'));
        st.set_focus(Some("t-gamma".into()));
        open_decisions(&mut st);
        for c in ['s', '1', 'x', 'q'] {
            assert!(st.handle_key(key(KeyCode::Char(c))).is_empty(), "{c}");
        }
        st.handle_paste("pasted");
        assert!(st.view.input.is_empty());
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Decisions);
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.screen, Screen::Grid);
        assert_eq!(st.focused(), Some("t-gamma"));
        st.handle_key(ctrl('g'));
        st.dive("t-beta");
        st.set_focus(Some("t-alpha".into()));
        open_decisions(&mut st);
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.screen, Screen::Stack);
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        assert_eq!(st.focused(), Some("t-alpha"));
        // `d` and `f` are text out of it.
        st.view.input.clear();
        st.handle_key(key(KeyCode::Char('d')));
        st.handle_key(key(KeyCode::Char('f')));
        assert_eq!(st.view.screen, Screen::Stack);
        assert_eq!(st.view.input, "df");
        assert_eq!(st.view.decision_filter, DecisionFilter::default());
        assert_eq!(st.handle_key(ctrl('c')), [Action::Quit]);
    }

    #[test]
    fn a_new_decision_keeps_the_selected_row_selected() {
        let mut st = with_decisions();
        open_decisions(&mut st);
        st.handle_key(key(KeyCode::Down));
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.decision_rows()[st.decision_selected()].chosen, "success");
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(4000);
        let ev = decision(
            &mut h,
            None,
            DecisionSite::Route,
            DecisionProvider::Code,
            &["alpha"],
            "alpha",
            1.0,
            0,
        );
        control(&mut st, &[ev]);
        assert_eq!(st.decision_rows()[st.decision_selected()].chosen, "success");
        // At the top, the selection follows the newest row.
        st.handle_key(key(KeyCode::Home));
        control(
            &mut st,
            &[decision(
                &mut h,
                None,
                DecisionSite::Route,
                DecisionProvider::Code,
                &["beta"],
                "beta",
                1.0,
                0,
            )],
        );
        assert_eq!(st.decision_rows()[st.decision_selected()].chosen, "beta");
    }

    #[test]
    fn decisions_are_capped() {
        let mut st = AppState::new();
        let mut h = ThreadScript::new(HUB_THREAD);
        let evs: Vec<Event> = (0..DECISIONS_CAP + 150)
            .map(|i| {
                decision(
                    &mut h,
                    None,
                    DecisionSite::Route,
                    DecisionProvider::Code,
                    &["x"],
                    &format!("d{i}"),
                    1.0,
                    0,
                )
            })
            .collect();
        control(&mut st, &evs);
        assert_eq!(st.decisions.len(), DECISIONS_CAP);
        assert_eq!(st.decisions.seen(), (DECISIONS_CAP + 150) as u64);
        let rows = st.decision_rows();
        assert_eq!(rows[0].chosen, format!("d{}", DECISIONS_CAP + 149));
        assert_eq!(rows[DECISIONS_CAP - 1].chosen, "d150");
    }

    #[test]
    fn transcripts_keep_the_newest_entries_only() {
        let mut t = Transcript::new("t1");
        t.starts_after(100);
        assert_eq!(t.entries.len(), 1);
        assert_eq!(t.last_seq, 100);
        let ev = |seq: u64| {
            Event::new(
                "t1",
                seq,
                Some(1),
                twr_core::event::Source::Adapter("fake".into()),
                EventBody::AgentToolCall(AgentToolCall {
                    call_id: format!("c{seq}"),
                    tool: "Bash".into(),
                    input_summary: "ls".into(),
                    ..Default::default()
                }),
            )
        };
        // Replayed seqs up to the start are ignored.
        t.apply(&ev(100));
        assert_eq!(t.entries.len(), 1);
        let n = TRANSCRIPT_MAX_ENTRIES as u64 + 10;
        for seq in 101..=100 + n {
            t.apply(&ev(seq));
        }
        assert_eq!(t.entries.len(), TRANSCRIPT_MAX_ENTRIES);
        match t.entries.last().unwrap() {
            Entry::Tool { call_id, .. } => assert_eq!(*call_id, format!("c{}", 100 + n)),
            e => panic!("{e:?}"),
        }
        // Nothing to note when the stream starts at the beginning.
        let mut t = Transcript::new("t1");
        t.starts_after(0);
        assert!(t.entries.is_empty());
    }

    fn mods(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    /// Draws `st` at `w` x `h` the way the IO loop does: the width and the panes are kept.
    /// Returns the screen.
    pub(crate) fn draw(st: &mut AppState, w: u16, h: u16) -> Buffer {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        (st.view.width, st.view.height) = (w, h);
        // As the IO loop does before it draws.
        st.settle_order(Instant::now());
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut panes = Panes::default();
        term.draw(|f| panes = crate::ui::render(f, st)).unwrap();
        st.note_panes(panes);
        term.backend().buffer().clone()
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Presses at `from`, drags to `to`, releases there; returns the release's actions.
    fn drag(st: &mut AppState, screen: &Buffer, from: (u16, u16), to: (u16, u16)) -> Vec<Action> {
        let left = MouseButton::Left;
        assert!(st
            .handle_mouse(mouse(MouseEventKind::Down(left), from.0, from.1), screen)
            .is_empty());
        assert!(st
            .handle_mouse(mouse(MouseEventKind::Drag(left), to.0, to.1), screen)
            .is_empty());
        st.handle_mouse(mouse(MouseEventKind::Up(left), to.0, to.1), screen)
    }

    /// The input line's text before and after the cursor.
    fn split(l: &LineInput) -> (&str, &str) {
        l.as_str().split_at(l.cursor())
    }

    #[test]
    fn the_input_line_edits_at_the_cursor() {
        let mut st = three_threads();
        st.view.width = 80;
        st.set_focus(Some("t-beta".into()));
        for c in "deploy the api".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        // Left/Right move the cursor (not the selection).
        let focus = st.focused().map(str::to_string);
        st.handle_key(key(KeyCode::Left));
        st.handle_key(key(KeyCode::Left));
        st.handle_key(key(KeyCode::Left));
        assert_eq!(st.focused().map(str::to_string), focus);
        assert_eq!(split(&st.view.input), ("deploy the ", "api"));
        st.handle_key(key(KeyCode::Right));
        st.handle_key(key(KeyCode::Char('X')));
        assert_eq!(split(&st.view.input), ("deploy the aX", "pi"));
        st.handle_key(key(KeyCode::Backspace));
        st.handle_key(key(KeyCode::Delete));
        assert_eq!(split(&st.view.input), ("deploy the a", "i"));
        // Home/End and Ctrl-A/E.
        st.handle_key(key(KeyCode::Home));
        assert_eq!(st.view.input.cursor(), 0);
        st.handle_key(key(KeyCode::End));
        assert_eq!(st.view.input.cursor(), st.view.input.len());
        st.handle_key(ctrl('a'));
        assert_eq!(st.view.input.cursor(), 0);
        st.handle_key(ctrl('e'));
        assert_eq!(st.view.input.cursor(), st.view.input.len());
        // By word: Ctrl and Alt with the arrows, Alt-B/F.
        st.handle_key(mods(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(split(&st.view.input).1, "ai");
        st.handle_key(mods(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(split(&st.view.input).1, "the ai");
        st.handle_key(mods(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(split(&st.view.input).1, "deploy the ai");
        st.handle_key(mods(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(split(&st.view.input).0, "deploy");
        st.handle_key(mods(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(split(&st.view.input).0, "deploy the");
        // Ctrl-W deletes the word before the cursor, Ctrl-K to the end.
        st.handle_key(ctrl('w'));
        assert_eq!(split(&st.view.input), ("deploy ", " ai"));
        st.handle_key(ctrl('k'));
        assert_eq!(st.view.input, "deploy ");
        st.handle_key(mods(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(st.view.input, "");
        for c in "fix tests".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        st.handle_key(ctrl('a'));
        for c in "now ".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        // Enter sends the whole line, wherever the cursor is.
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Say {
                text: "now fix tests".into(),
                // Nothing drawn here: below beta's permission ([`AppState::seen_gseq`]).
                seen_gseq: st.seen_gseq(),
            }]
        );
        assert_eq!((st.view.input.as_str(), st.view.input.cursor()), ("", 0));
        // Ctrl-U clears the line; unknown Alt keys type nothing.
        for c in "abc".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        st.handle_key(key(KeyCode::Left));
        st.handle_key(mods(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(st.view.input, "abc");
        st.handle_key(ctrl('u'));
        assert_eq!(st.view.input, "");
        assert_eq!(st.focused(), Some("t-beta"), "editing moved no selection");
    }

    /// The Shift-arrows move the selection, whatever is typed, and edit nothing: ↑↓ between
    /// the cards, ← to the conversation, → back to the cards. Plain ←→ move the cursor.
    #[test]
    fn shift_arrows_move_the_selection_and_arrows_the_cursor() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        typed(&mut st, "ab");
        st.handle_key(shift(KeyCode::Down));
        assert_eq!(st.focused(), Some("t-alpha"));
        st.handle_key(shift(KeyCode::Up));
        assert_eq!(st.focused(), Some("t-beta"));
        st.handle_key(shift(KeyCode::Left));
        assert!(st.main_focused());
        assert_eq!(st.focused(), Some("t-beta"));
        st.handle_key(shift(KeyCode::Right));
        assert!(!st.main_focused());
        assert_eq!(split(&st.view.input), ("ab", ""));
        st.handle_key(key(KeyCode::Left));
        assert_eq!(split(&st.view.input), ("a", "b"));
        assert_eq!(st.focused(), Some("t-beta"));
        // Shift-→ with no card selected selects the first.
        st.set_focus(None);
        st.handle_key(shift(KeyCode::Left));
        st.handle_key(shift(KeyCode::Right));
        assert_eq!(st.focused(), Some("t-beta"));
    }

    /// A paste is text, never keys: a pasted "1\n" neither answers the card nor sends, even
    /// with its action popup open (the paste closes it): it is typed, its line break kept.
    #[test]
    fn a_paste_is_text_never_keys() {
        let mut st = three_threads();
        st.view.width = 80;
        st.set_focus(Some("t-beta".into()));
        popup_open(&mut st);
        st.handle_paste("1\nyes");
        assert_eq!(st.view.action, None, "the paste closed the popup");
        assert_eq!(st.view.input, "1\nyes");
        st.handle_key(ctrl('a'));
        st.handle_paste("say ");
        assert_eq!(split(&st.view.input), ("say ", "1\nyes"));
        // A paste cancels a pending confirmation and is dropped, like a key.
        st.confirm_lifecycle_of(true, None);
        assert!(st.view.confirm.is_some());
        st.handle_paste("y");
        assert!(st.view.confirm.is_none());
        assert_eq!(st.view.status.as_deref(), Some("cancelled"));
        assert_eq!(st.view.input, "say 1\nyes");
    }

    /// Shift-Enter breaks the line, and so do Alt-Enter and Ctrl-J (for terminals that send
    /// Shift-Enter as Enter); Enter sends every line, to the orchestrator
    /// or in a dive to the thread. The popup never takes a line break, and a recalled line
    /// keeps its breaks.
    #[test]
    fn shift_enter_breaks_the_line_and_enter_sends_it() {
        let mut st = three_threads();
        st.view.width = 80;
        let typed = |st: &mut AppState, s: &str| {
            for c in s.chars() {
                st.handle_key(key(KeyCode::Char(c)));
            }
        };
        typed(&mut st, "first");
        assert!(st
            .handle_key(mods(KeyCode::Enter, KeyModifiers::SHIFT))
            .is_empty());
        typed(&mut st, "second");
        assert!(st
            .handle_key(mods(KeyCode::Enter, KeyModifiers::ALT))
            .is_empty());
        typed(&mut st, "third");
        assert!(st.handle_key(ctrl('j')).is_empty());
        typed(&mut st, "fourth");
        st.handle_key(ctrl('j'));
        typed(&mut st, "fifth");
        assert_eq!(st.view.input, "first\nsecond\nthird\nfourth\nfifth");
        // In the middle of a line: broken at the cursor.
        st.handle_key(key(KeyCode::Left));
        st.handle_key(key(KeyCode::Left));
        st.handle_key(mods(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(
            split(&st.view.input),
            ("first\nsecond\nthird\nfourth\nfif\n", "th")
        );
        st.handle_key(key(KeyCode::Backspace));
        // Enter sends all of it, wherever the cursor is; blank lines around it go.
        st.handle_key(ctrl('a'));
        st.handle_key(ctrl('j'));
        st.handle_key(ctrl('e'));
        st.handle_key(ctrl('j'));
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Say {
                text: "first\nsecond\nthird\nfourth\nfifth".into(),
                seen_gseq: st.seen_gseq(),
            }]
        );
        assert!(st.view.input.is_empty());
        // Up recalls it with its lines; the cursor at its end, Up goes up its rows first.
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.view.input, "first\nsecond\nthird\nfourth\nfifth");
        st.handle_key(key(KeyCode::Up));
        assert_eq!(
            split(&st.view.input).1,
            "h\nfifth",
            "a row up, not the history"
        );
        st.handle_key(ctrl('u'));
        // Only Ctrl-J: Ctrl-Alt-J does not break; a plain Enter after a `\` sends.
        typed(&mut st, "a");
        st.handle_key(mods(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(st.view.input, "a");
        st.handle_key(ctrl('u'));
        typed(&mut st, "C:\\");
        assert!(matches!(
            st.handle_key(key(KeyCode::Enter)).as_slice(),
            [Action::Say { text, .. }] if text == "C:\\"
        ));
        // With the @ popup open, Shift-Enter breaks the line instead of completing.
        typed(&mut st, "ask @al");
        assert!(st.popup().is_some());
        st.handle_key(mods(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(st.view.input, "ask @al\n");
        assert!(st.popup().is_none());
        st.handle_key(ctrl('u'));
        // In a dive the lines go to the thread.
        st.set_focus(Some("t-alpha".into()));
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.dive.as_deref(), Some("t-alpha"));
        typed(&mut st, "one");
        st.handle_key(mods(KeyCode::Enter, KeyModifiers::SHIFT));
        typed(&mut st, "two");
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Send {
                thread: "t-alpha".into(),
                text: "one\ntwo".into(),
            }]
        );
        // Review (Shift-Enter reports): a Shift-Enter that comes in says the terminal tells it
        // apart; Alt-Enter and Ctrl-J say nothing.
        assert!(st.view.shift_enter, "the Shift-Enters above");
        let mut fresh = three_threads();
        fresh.handle_key(mods(KeyCode::Enter, KeyModifiers::ALT));
        fresh.handle_key(ctrl('j'));
        assert!(!fresh.view.shift_enter);
        fresh.handle_key(mods(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(fresh.view.shift_enter);
    }

    /// Each line of the input box starts a row: the box grows with the lines up to its cap,
    /// then scrolls; a line break is never drawn, and a copy keeps it.
    #[test]
    fn the_box_grows_with_its_lines() {
        let mut st = three_threads();
        st.handle_key(key(KeyCode::Esc));
        draw_cursor(&mut st, 40, 30);
        st.handle_paste("one\ntwo\n\nfour");
        let (buf, cur) = draw_cursor(&mut st, 40, 30);
        let (rows, input) = input_rows(&st, &buf);
        let rows: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
        assert_eq!(rows, ["one", "two", "", "four"]);
        assert_eq!(cur, (input.x + 4, input.y + 3));
        // A drag over the rows copies the lines.
        assert_eq!(
            drag(
                &mut st,
                &buf,
                (input.x, input.y),
                (input.right() - 1, input.y + 3)
            ),
            [Action::Copy {
                text: "one\ntwo\n\nfour".into()
            }]
        );
        // Past the cap (6 rows), the rows around the cursor.
        for _ in 0..4 {
            st.handle_key(mods(KeyCode::Enter, KeyModifiers::SHIFT));
        }
        st.handle_key(key(KeyCode::Char('x')));
        let (buf, cur) = draw_cursor(&mut st, 40, 30);
        let (rows, input) = input_rows(&st, &buf);
        assert_eq!(input.height, 6);
        assert_eq!(rows[1].trim_end(), "four");
        assert_eq!(rows[5].trim_end(), "x");
        assert_eq!(cur, (input.x + 1, input.y + 5));
        st.handle_key(ctrl('a'));
        let (buf, _) = draw_cursor(&mut st, 40, 30);
        assert_eq!(input_rows(&st, &buf).0[0].trim_end(), "one");
    }

    /// The rows of the input box as drawn in `buf`, and the box.
    fn input_rows(st: &AppState, buf: &Buffer) -> (Vec<String>, ratatui::layout::Rect) {
        let a = st.view.panes.get(&PaneId::Input).unwrap().area;
        let rows = (a.y..a.bottom())
            .map(|y| (a.x..a.right()).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        (rows, a)
    }

    /// Draws `st` at `w` x `h` like [`draw`]; returns the screen and the terminal cursor.
    fn draw_cursor(st: &mut AppState, w: u16, h: u16) -> (Buffer, (u16, u16)) {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        (st.view.width, st.view.height) = (w, h);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut panes = Panes::default();
        term.draw(|f| panes = crate::ui::render(f, st)).unwrap();
        st.note_panes(panes);
        let cur = term.get_cursor_position().unwrap();
        (term.backend().buffer().clone(), (cur.x, cur.y))
    }

    /// A line wider than the box wraps onto more rows: the box grows up to its cap and the
    /// panes above give up the rows; past the cap it scrolls to the cursor's row. The terminal
    /// cursor sits on the right row and cell.
    #[test]
    fn a_long_line_wraps_and_the_box_grows() {
        let mut st = three_threads();
        st.handle_key(key(KeyCode::Esc));
        let (_, (_, y0)) = draw_cursor(&mut st, 40, 30);
        let one = st.view.panes.get(&PaneId::Input).unwrap().area;
        assert_eq!((one.height, y0), (1, one.y), "one row while it fits");
        let text: String = (0..60).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        st.handle_paste(&text);
        // 40 columns: 38 inside the box; the rest on a second row, the cursor after it.
        let (buf, cur) = draw_cursor(&mut st, 40, 30);
        let (rows, input) = input_rows(&st, &buf);
        assert_eq!(
            rows,
            [text[..38].to_string(), format!("{:38}", &text[38..])]
        );
        assert_eq!(input.y + 1, one.y, "the box grew upwards by one row");
        assert_eq!(cur, (input.x + 22, input.y + 1));
        // Home goes to the start of the cursor's row, Ctrl-A to the start of the text.
        st.handle_key(key(KeyCode::Home));
        assert_eq!(draw_cursor(&mut st, 40, 30).1, (input.x, input.y + 1));
        st.handle_key(ctrl('a'));
        assert_eq!(draw_cursor(&mut st, 40, 30).1, (input.x, input.y));
        // Up and Down move between the rows, keeping the column.
        st.handle_key(ctrl('e'));
        st.handle_key(key(KeyCode::Up));
        assert_eq!(split(&st.view.input).0, &text[..22]);
        assert_eq!(draw_cursor(&mut st, 40, 30).1, (input.x + 22, input.y));
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.view.input.cursor(), 60);
        // Up to the cap (a third of 30 rows is more than 6), then the rows around the cursor.
        st.handle_paste(&"x".repeat(400));
        let (buf, cur) = draw_cursor(&mut st, 40, 30);
        let (rows, input) = input_rows(&st, &buf);
        assert_eq!(input.height, 6);
        assert_eq!(
            cur,
            (input.x + 4, input.y + 5),
            "460 cells: 12 full rows and 4"
        );
        assert_eq!(rows[5].trim_end(), "xxxx");
        assert!(
            st.view
                .panes
                .get(&PaneId::Transcript)
                .unwrap()
                .area
                .bottom()
                < input.y
        );
        st.handle_key(ctrl('a'));
        let (buf, cur) = draw_cursor(&mut st, 40, 30);
        assert_eq!(input_rows(&st, &buf).0[0], text[..38]);
        assert_eq!(cur, (input.x, input.y));
        // A short screen caps the box at a third of it.
        let (_, _) = draw_cursor(&mut st, 40, 12);
        assert_eq!(st.view.panes.get(&PaneId::Input).unwrap().area.height, 4);
        // Wide characters wrap by their width and never split: 19 on a 38-cell row.
        st.handle_key(ctrl('u'));
        st.handle_paste(&"日本".repeat(15));
        let (buf, cur) = draw_cursor(&mut st, 40, 30);
        let (rows, input) = input_rows(&st, &buf);
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|r| r.starts_with('日') || r.starts_with('本')));
        assert_eq!(cur, (input.x + 22, input.y + 1));
        // A narrow screen: every row within the box, the cursor on the last one.
        st.handle_key(ctrl('u'));
        st.handle_paste("deploy the api to staging, then run the smoke tests");
        let (buf, cur) = draw_cursor(&mut st, 20, 30);
        let (rows, input) = input_rows(&st, &buf);
        let rows: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
        assert_eq!(
            rows,
            ["deploy the api to", "staging, then run", "the smoke tests"]
        );
        assert_eq!(cur, (input.x + 15, input.y + 2));
    }

    /// Up and Down move the cursor between rows only while the `/` popup is closed; while it
    /// is open, they pick in it.
    #[test]
    fn the_popup_keeps_its_arrows() {
        let mut st = three_threads();
        st.handle_key(key(KeyCode::Esc));
        draw_cursor(&mut st, 40, 30);
        st.handle_key(key(KeyCode::Char('/')));
        assert!(st.popup().is_some());
        st.handle_key(key(KeyCode::Down));
        assert_eq!((st.view.popup_sel, st.view.input.cursor()), (1, 1));
        st.handle_key(key(KeyCode::Up));
        assert_eq!((st.view.popup_sel, st.view.input.cursor()), (0, 1));
        // Closed (Esc), the arrows are the line's again.
        st.handle_key(key(KeyCode::Esc));
        assert!(st.popup().is_none());
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.view.input.cursor(), 0);
    }

    /// A click on a wrapped row puts the cursor there; a drag over the rows copies the text
    /// as one line.
    #[test]
    fn the_mouse_works_on_wrapped_rows() {
        let mut st = three_threads();
        st.handle_key(key(KeyCode::Esc));
        draw_cursor(&mut st, 20, 30);
        st.handle_paste("deploy the api to staging, then run the smoke tests");
        let (screen, _) = draw_cursor(&mut st, 20, 30);
        let input = st.view.panes.get(&PaneId::Input).unwrap().area;
        assert_eq!(input.height, 3);
        assert!(drag(
            &mut st,
            &screen,
            (input.x + 5, input.y + 1),
            (input.x + 5, input.y + 1)
        )
        .is_empty());
        assert_eq!(split(&st.view.input).0, "deploy the api to stagi");
        // Past the text of a row: its end, before the space it wraps at.
        drag(
            &mut st,
            &screen,
            (input.x + 17, input.y),
            (input.x + 17, input.y),
        );
        assert_eq!(split(&st.view.input).0, "deploy the api to");
        let (screen, cur) = draw_cursor(&mut st, 20, 30);
        assert_eq!(cur, (input.x + 17, input.y));
        // A drag from the first cell to the last copies the line, rows joined by a space.
        assert_eq!(
            drag(
                &mut st,
                &screen,
                (input.x, input.y),
                (input.right() - 1, input.y + 2)
            ),
            [Action::Copy {
                text: "deploy the api to staging, then run the smoke tests".into()
            }]
        );
        // A word cut at the edge is copied whole.
        st.handle_key(ctrl('u'));
        st.handle_paste(&"z".repeat(25));
        let (screen, _) = draw_cursor(&mut st, 20, 30);
        let input = st.view.panes.get(&PaneId::Input).unwrap().area;
        assert_eq!(
            drag(
                &mut st,
                &screen,
                (input.x, input.y),
                (input.right() - 1, input.y + 1)
            ),
            [Action::Copy {
                text: "z".repeat(25)
            }]
        );
    }

    /// `alpha` focused, with a transcript whose reply wraps over several rows.
    fn with_transcript(w: u16) -> (AppState, Buffer) {
        let mut st = three_threads();
        st.dive("t-alpha");
        let mut a = ThreadScript::new("t-alpha");
        a.at(50);
        for ev in [
            a.turn_started("explain"),
            a.text("The quick brown fox jumps over the lazy dog and keeps running far away."),
            a.text("second"),
        ] {
            st.apply_event(&thread_item(&ev));
        }
        let screen = draw(&mut st, w, 30);
        (st, screen)
    }

    #[test]
    fn a_drag_copies_the_transcript_text_without_borders() {
        let (mut st, screen) = with_transcript(40);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        // From the transcript's first cell, dragged past its right edge and down past its
        // bottom: the whole pane, nothing else (not the input box under it).
        let actions = drag(&mut st, &screen, (tr.area.x, tr.area.y), (200, 200));
        let [Action::Copy { text }] = actions.as_slice() else {
            panic!("{actions:?}");
        };
        assert_eq!(
            *text,
            "explain\n\nThe quick brown fox jumps over the lazy dog and keeps running far \
             away.\n\nsecond"
        );
        assert!(!text.contains('│'), "no border: {text}");
        let n = text.chars().count();
        assert_eq!(
            st.view.status.as_deref(),
            Some(format!("copied {n} characters").as_str())
        );
        // The selection stays highlighted until the next key or click.
        assert!(st.view.selection.as_ref().is_some_and(|s| s.done));
        let shown = draw(&mut st, 40, 30);
        assert!(shown[(tr.area.x, tr.area.y)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED));
        // Not the header band above it.
        assert!(!shown[(tr.area.x, tr.area.y - 1)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED));
        st.handle_key(key(KeyCode::Char('?')));
        assert!(st.view.selection.is_none());
    }

    /// Copying a working card leaves its icon out: the frame changes with the clock and is
    /// not the card's text.
    #[test]
    fn a_copied_card_has_no_icon() {
        let mut st = three_threads();
        st.view.animate = true;
        st.view.frame = 4;
        let screen = draw(&mut st, 120, 40);
        let a = st
            .view
            .panes
            .get(&PaneId::Thread("t-gamma".into()))
            .unwrap()
            .area;
        let actions = drag(&mut st, &screen, (a.x, a.y), (a.right(), a.y + 1));
        let [Action::Copy { text }] = &actions[..] else {
            panic!("{actions:?}");
        };
        // Its status row, without the icon, then the blank row under it.
        assert_eq!(text.trim_end(), "running", "{text:?}");
    }

    /// Copying the conversation leaves the working cursor out, at the end of a reply
    /// streaming in or on its own row.
    #[test]
    fn a_copied_conversation_has_no_cursor() {
        let mut st = without_worker_requests(with_orch());
        let mut o = ThreadScript::new("t-orch");
        o.at(200);
        control(&mut st, &[o.turn_started("hi")]);
        st.view.animate = true;
        st.view.frame = 6;
        for (open, want) in [(true, "hi\n\nOn it"), (false, "hi\n\nOn it")] {
            st.transcript.as_mut().unwrap().entries = vec![
                Entry::TurnStart {
                    seq: 0,
                    turn: Some(1),
                    from: TurnFrom::User,
                    prompt: "hi".into(),
                    notice: false,
                    withdrawn: false,
                },
                Entry::Text {
                    seq: 0,
                    text: "On it".into(),
                    open,
                },
            ];
            st.view.selection = None;
            let screen = draw(&mut st, 120, 30);
            let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
            let actions = drag(&mut st, &screen, (a.x, a.y), (a.right(), a.y + 2));
            let [Action::Copy { text }] = &actions[..] else {
                panic!("{actions:?}");
            };
            assert_eq!(text, want, "open: {open}");
        }
    }

    #[test]
    fn a_click_copies_nothing_and_focuses_what_it_hits() {
        let (mut st, screen) = with_transcript(120);
        // On the transcript: nothing.
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        assert!(drag(&mut st, &screen, (tr.x + 2, tr.y), (tr.x + 2, tr.y)).is_empty());
        assert!(st.view.selection.is_none() && st.view.status.is_none());
        assert!(st.main_focused(), "the click selects the conversation");
        // On beta's card: beta gets focus.
        let beta = st
            .view
            .panes
            .get(&PaneId::Thread("t-beta".into()))
            .unwrap()
            .area;
        assert!(drag(
            &mut st,
            &screen,
            (beta.x + 1, beta.y + 1),
            (beta.x + 1, beta.y + 1)
        )
        .is_empty());
        assert_eq!(st.focused(), Some("t-beta"));
        assert!(!st.main_focused());
        // On the input line: the cursor goes where it was clicked; the card stays selected.
        st.view.input = "hello world".into();
        let screen = draw(&mut st, 120, 30);
        let input = st.view.panes.get(&PaneId::Input).unwrap().area;
        drag(
            &mut st,
            &screen,
            (input.x + 6, input.y),
            (input.x + 6, input.y),
        );
        assert_eq!(split(&st.view.input), ("hello ", "world"));
        assert_eq!(st.focused(), Some("t-beta"));
        // A drag over spaces only copies nothing.
        let empty = drag(
            &mut st,
            &screen,
            (input.x + 50, input.y),
            (input.x + 60, input.y),
        );
        assert!(empty.is_empty(), "{empty:?}");
        // On a border: nothing starts.
        assert!(st
            .handle_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
                &screen
            )
            .is_empty());
        assert!(st.view.selection.is_none());
    }

    /// A click selects a card; a second one on it soon after dives in. A slow second click,
    /// or one on another card, only selects.
    #[test]
    fn a_double_click_on_a_card_dives_in() {
        let mut st = with_orch();
        let screen = draw(&mut st, 120, 40);
        let at = |st: &AppState, id: &str| {
            let a = st.view.panes.get(&PaneId::Thread(id.into())).unwrap().area;
            (a.x + 1, a.y + 1)
        };
        let beta = at(&st, "t-beta");
        let gamma = at(&st, "t-gamma");
        assert!(drag(&mut st, &screen, beta, beta).is_empty());
        assert_eq!(
            (st.focused(), st.view.dive.as_deref()),
            (Some("t-beta"), None)
        );
        drag(&mut st, &screen, gamma, gamma);
        assert_eq!(
            (st.focused(), st.view.dive.as_deref()),
            (Some("t-gamma"), None)
        );
        drag(&mut st, &screen, gamma, gamma);
        assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
        st.back();
        drag(&mut st, &screen, beta, beta);
        st.view.last_click = Some((
            "t-beta".into(),
            Instant::now() - DOUBLE_CLICK - Duration::from_millis(50),
        ));
        drag(&mut st, &screen, beta, beta);
        assert_eq!(st.view.dive, None, "too slow for a double click");
    }

    /// The orchestrator's conversation in [`with_orch`]: the user asks, the orchestrator checks
    /// with two twr tools and answers; then a hub notice turn and its answer.
    pub(crate) fn orch_conversation(st: &mut AppState) {
        let mut o = ThreadScript::new("t-orch");
        o.at(100);
        let tool = |o: &mut ThreadScript, id: &str, name: &str, ok: bool| {
            [
                o.event(EventBody::AgentToolCall(AgentToolCall {
                    call_id: id.into(),
                    tool: name.into(),
                    input_summary: "{}".into(),
                    ..Default::default()
                })),
                o.event(EventBody::AgentToolResult(AgentToolResult {
                    call_id: id.into(),
                    ok,
                    ..Default::default()
                })),
            ]
        };
        let mut evs = vec![o.turn_started("which threads need me?")];
        evs.extend(tool(&mut o, "c1", "mcp__twr__list_threads", true));
        evs.extend(tool(&mut o, "c2", "mcp__twr__attention_list", true));
        evs.push(o.text("beta waits for a permission to run rm -rf target; alpha gave up."));
        evs.push(o.turn_ended(TurnResult::Completed));
        let mut notice = o.turn_started("[twr notice] 1 update from the hub: gamma finished.");
        if let EventBody::TurnStarted(p) = &mut notice.body {
            p.from = TurnFrom::Orchestrator;
            p.ext = serde_json::json!({"twr": {"notice": true}})
                .as_object()
                .cloned();
        }
        evs.push(notice);
        evs.extend(tool(&mut o, "c3", "mcp__twr__thread_report", false));
        evs.push(o.text("gamma finished its tests: all green."));
        evs.push(o.turn_ended(TurnResult::Completed));
        for ev in &evs {
            st.apply_event(&thread_item(ev));
        }
    }

    /// Ctrl-O shows and hides the orchestrator's tool calls and the hub's notices; a click on
    /// a folded group's line opens that group; a drag copies what is shown.
    #[test]
    fn ctrl_o_and_a_click_expand_the_orchestrators_internals() {
        let mut st = without_worker_requests(with_orch());
        orch_conversation(&mut st);
        let text = |st: &mut AppState| {
            let buf = draw(st, 120, 40);
            let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
            (a.y..a.bottom())
                .map(|y| {
                    (a.x..a.right())
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
        };
        // No line after a completed turn: the replies follow one another.
        let folded = text(&mut st);
        assert_eq!(
            folded,
            [
                "❯ which threads need me?",
                "  Listed the threads, used 1 twr tool (ctrl+o to expand)",
                "● beta waits for a permission to run rm -rf target; alpha gave up.",
                "  hub update (ctrl+o to expand)",
                "  Used 1 twr tool, 1 failed (ctrl+o to expand)",
                "● gamma finished its tests: all green.",
            ]
        );
        // Ctrl-O: everything; again: folded.
        st.handle_key(ctrl('o'));
        let open = text(&mut st);
        assert!(
            open.contains(&"● twr list_threads({})".to_string()),
            "{open:?}"
        );
        assert!(
            open.contains(&"  [twr notice] 1 update from the hub: gamma finished.".to_string()),
            "{open:?}"
        );
        assert!(st.view.status.as_deref().unwrap().contains("showing"));
        st.handle_key(ctrl('o'));
        assert_eq!(text(&mut st), folded);
        // A click on a marker opens that group only.
        let screen = draw(&mut st, 120, 40);
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let row = a.y + 2;
        assert_eq!(
            st.view.panes.get(&PaneId::Transcript).unwrap().link(row),
            Some("tools:c1")
        );
        assert!(drag(&mut st, &screen, (a.x + 3, row), (a.x + 3, row)).is_empty());
        let one = text(&mut st);
        assert!(
            one.contains(&"● twr attention_list({})".to_string()),
            "{one:?}"
        );
        assert!(
            one.contains(&"  hub update (ctrl+o to expand)".to_string()),
            "{one:?}"
        );
        // A drag copies the lines as shown.
        let screen = draw(&mut st, 120, 40);
        let copied = drag(&mut st, &screen, (a.x, a.y), (a.x + 60, a.y + 4));
        let [Action::Copy { text: got }] = copied.as_slice() else {
            panic!("{copied:?}");
        };
        assert!(got.contains("twr list_threads({})"), "{got}");
        // A dive folds a worker's tool calls the same way; Ctrl-O shows them. Its turns keep
        // their end lines.
        st.dive("t-alpha");
        let mut a2 = ThreadScript::new("t-alpha");
        a2.at(200);
        let start = a2.turn_started("run the tests");
        let call = a2.event(EventBody::AgentToolCall(AgentToolCall {
            call_id: "w1".into(),
            tool: "Bash".into(),
            input_summary: "cargo test".into(),
            ..Default::default()
        }));
        let evs = [
            start,
            call,
            a2.text("all green"),
            a2.turn_ended(TurnResult::Completed),
        ];
        for ev in &evs {
            st.apply_event(&thread_item(ev));
        }
        let dive = text(&mut st);
        assert!(
            dive.contains(&"  Ran 1 shell command… (ctrl+o to expand)".to_string()),
            "{dive:?}"
        );
        assert!(!dive.contains(&"● Bash(cargo test)".to_string()));
        assert!(dive.contains(&"✻ Done".to_string()), "{dive:?}");
        st.handle_key(ctrl('o'));
        assert_eq!(
            st.view.status.as_deref(),
            Some(
                "showing tool calls and the hub's updates, here and in every thread (Ctrl-O \
                 folds them)"
            )
        );
        let open = text(&mut st);
        assert!(open.contains(&"● Bash(cargo test)".to_string()), "{open:?}");
        st.handle_key(ctrl('o'));
        assert_eq!(text(&mut st), dive);
        // A click on its marker opens it.
        let screen = draw(&mut st, 120, 40);
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let row = (a.y..a.bottom())
            .find(|y| st.view.panes.get(&PaneId::Transcript).unwrap().link(*y) == Some("tools:w1"))
            .expect("the dive's marker links its group");
        assert!(drag(&mut st, &screen, (a.x + 3, row), (a.x + 3, row)).is_empty());
        assert!(text(&mut st).contains(&"● Bash(cargo test)".to_string()));
    }

    /// In the orchestrator conversation a turn that did not complete still ends with its line.
    #[test]
    fn the_orchestrator_view_keeps_the_end_of_a_failed_turn() {
        let mut st = without_worker_requests(with_orch());
        let mut o = ThreadScript::new("t-orch");
        o.at(100);
        let evs = [
            o.turn_started("deploy"),
            o.text("on it"),
            o.turn_ended(TurnResult::Error),
            o.turn_started("again"),
            o.text("stopped"),
            o.turn_ended(TurnResult::Interrupted),
            o.turn_started("and now?"),
            o.text("done"),
            o.turn_ended(TurnResult::Completed),
        ];
        for ev in &evs {
            st.apply_event(&thread_item(ev));
        }
        let buf = draw(&mut st, 120, 40);
        let a = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let rows: Vec<String> = (a.y..a.bottom())
            .map(|y| {
                (a.x..a.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|l| !l.is_empty())
            .collect();
        assert!(
            rows.iter().any(|r| r.starts_with("✗ The turn failed")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.starts_with("! Interrupted")),
            "{rows:?}"
        );
        assert!(!rows.iter().any(|r| r.starts_with("✻")), "{rows:?}");
        assert_eq!(rows.last().map(String::as_str), Some("● done"));
    }

    /// `thread.mode_changed` reads as a line of the transcript, by kind (whether twr-core has
    /// a type for it or not).
    #[test]
    fn a_mode_change_is_a_transcript_line() {
        let mut s = ThreadScript::new("t1");
        let mut tr = Transcript::new("t1");
        for (data, want) in [
            (
                serde_json::json!({"mode": "auto", "previous": "manual", "by": "user"}),
                "mode changed from manual to auto (by you)",
            ),
            (
                serde_json::json!({"mode": "plan", "by": "orchestrator"}),
                "mode changed to plan (by the orchestrator)",
            ),
            // `by` defaults to the user.
            (
                serde_json::json!({"mode": "auto"}),
                "mode changed to auto (by you)",
            ),
            // The user chose the mode the thread had.
            (
                serde_json::json!({"mode": "manual", "previous": "manual", "by": "user"}),
                "mode kept at manual (by you)",
            ),
        ] {
            let body = EventBody::from_kind_data(MODE_CHANGED, data).unwrap();
            assert_eq!(body.kind(), MODE_CHANGED);
            tr.apply(&s.event(body));
            assert_eq!(
                tr.entries.last(),
                Some(&Entry::Notice {
                    seq: tr.last_seq,
                    level: NoticeLevel::Info,
                    text: want.into()
                })
            );
        }
        // The runtime the hub restarts for the new mode did not stop for good.
        let mut stop = s.event(EventBody::ThreadStopped(Default::default()));
        if let EventBody::ThreadStopped(p) = &mut stop.body {
            p.ext = serde_json::json!({"twr": {"reason": "mode_change"}})
                .as_object()
                .cloned();
        }
        tr.apply(&stop);
        assert!(matches!(
            tr.entries.last(),
            Some(Entry::Notice { text, .. }) if text == "restarting the runtime in its new mode"
        ));
    }

    /// `thread.model_changed` reads as a line of the transcript naming what changed, from what,
    /// and by whom; the runtime the hub restarts for it did not stop, and any other reason (or
    /// none) is a stop.
    #[test]
    fn a_model_change_is_a_transcript_line() {
        use twr_core::event::{ChangedBy, SessionPayload};
        let mut s = ThreadScript::new("t1");
        let mut tr = Transcript::new("t1");
        let opt = |v: Option<&str>| v.map(str::to_string);
        for ((model, effort, previous_model, previous_effort, by), want) in [
            (
                (
                    Some("claude-fable-5-1"),
                    Some("high"),
                    Some("sonnet"),
                    None,
                    ChangedBy::Orchestrator,
                ),
                "model changed from sonnet to claude-fable-5-1, effort from default to high \
                 (by the orchestrator)",
            ),
            (
                (
                    Some("opus"),
                    None,
                    Some("opus"),
                    Some("high"),
                    ChangedBy::User,
                ),
                "effort changed from high to default (by you)",
            ),
            (
                (
                    None,
                    Some("low"),
                    Some("opus"),
                    Some("low"),
                    ChangedBy::User,
                ),
                "model changed from opus to default (by you)",
            ),
            (
                (Some("opus"), None, Some("opus"), None, ChangedBy::User),
                "model kept at opus, effort at default (by you)",
            ),
        ] {
            tr.apply(&s.event(EventBody::ThreadModelChanged(ThreadModelChanged {
                model: opt(model),
                effort: opt(effort),
                previous_model: opt(previous_model),
                previous_effort: opt(previous_effort),
                by,
                ..Default::default()
            })));
            assert_eq!(
                tr.entries.last(),
                Some(&Entry::Notice {
                    seq: tr.last_seq,
                    level: NoticeLevel::Info,
                    text: want.into()
                })
            );
        }
        let stop = |s: &mut ThreadScript, reason: Option<&str>| {
            let ext = reason.and_then(|r| {
                serde_json::json!({"twr": {"reason": r}})
                    .as_object()
                    .cloned()
            });
            s.event(EventBody::ThreadStopped(SessionPayload {
                ext,
                ..Default::default()
            }))
        };
        for (reason, want) in [
            (
                Some(MODEL_RESTART_REASON),
                "restarting the runtime with its new settings",
            ),
            (
                Some(MODE_RESTART_REASON),
                "restarting the runtime in its new mode",
            ),
            (
                Some(twr_core::event::TRIM_RESTART_REASON),
                "the runtime stopped for a trim of its build output; the next prompt resumes the session",
            ),
            (
                Some(twr_core::event::BOX_UPGRADE_REASON),
                "restarting on the box's new twr build; the next prompt resumes the session",
            ),
            (Some("hub_shutdown"), "thread stopped"),
            (None, "thread stopped"),
        ] {
            tr.apply(&stop(&mut s, reason));
            assert!(
                matches!(tr.entries.last(), Some(Entry::Notice { text, .. }) if text == want),
                "{reason:?}: {:?}",
                tr.entries.last()
            );
        }
    }

    /// `/model <thread>` shows the thread's model and effort; with a model, `--effort` or both
    /// it sets them as the user (`thread.set_model`, an omitted field left out so the hub keeps
    /// it). In a dive the thread may be left out only when no model is given.
    #[test]
    fn model_shows_and_sets_a_threads_model_and_effort() {
        let mut st = with_orch();
        let set = |thread: &str, model: Option<&str>, effort: Option<&str>| Action::SetModel {
            thread: thread.into(),
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            runtime: None,
        };
        // Showing changes nothing and clears the line.
        assert!(type_line(&mut st, "/model beta").is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("beta: model default, effort default")
        );
        assert_eq!(st.view.input, "");
        let actions = type_line(&mut st, "/model beta claude-fable-5-1 --effort high");
        assert_eq!(
            actions,
            [set("t-beta", Some("claude-fable-5-1"), Some("high"))]
        );
        assert_eq!(
            crate::action_request(&actions[0]),
            Some((
                "thread.set_model",
                serde_json::json!({"thread": "t-beta", "model": "claude-fable-5-1",
                                   "effort": "high"})
            ))
        );
        assert_eq!(
            st.view.status.as_deref(),
            Some("beta → model claude-fable-5-1, effort high: waiting for the hub")
        );
        let actions = type_line(&mut st, "/model alpha opus");
        assert_eq!(actions, [set("t-alpha", Some("opus"), None)]);
        assert_eq!(
            crate::action_request(&actions[0]),
            Some((
                "thread.set_model",
                serde_json::json!({"thread": "t-alpha", "model": "opus"})
            ))
        );
        let actions = type_line(&mut st, "/model alpha --effort default");
        assert_eq!(actions, [set("t-alpha", None, Some("default"))]);
        assert_eq!(
            crate::action_request(&actions[0]),
            Some((
                "thread.set_model",
                serde_json::json!({"thread": "t-alpha", "effort": "default"})
            ))
        );
        assert_eq!(
            st.view.status.as_deref(),
            Some("alpha → effort default: waiting for the hub")
        );
        // Outside a dive the thread is needed; a failed command stays on the line.
        for line in ["/model", "/model --effort high"] {
            assert!(type_line(&mut st, line).is_empty());
            assert!(
                st.view
                    .status
                    .as_deref()
                    .unwrap()
                    .starts_with("usage: /model <thread>"),
                "{line}: {:?}",
                st.view.status
            );
            assert_eq!(st.view.input, line);
        }
        assert!(type_line(&mut st, "/model nope opus").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no thread nope"));
        assert!(type_line(&mut st, "/model beta x y").is_empty());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .starts_with("usage: /model"));
        // In a dive: the thread dived into, unless one is named.
        st.dive("t-gamma");
        assert_eq!(
            type_line(&mut st, "/model --effort low"),
            [set("t-gamma", None, Some("low"))]
        );
        assert!(type_line(&mut st, "/model").is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("gamma: model default, effort default")
        );
        assert_eq!(
            type_line(&mut st, "/model beta sonnet"),
            [set("t-beta", Some("sonnet"), None)]
        );
    }

    /// `/model <thread>` names the requested model and effort, and the model the runtime
    /// reports when it is another.
    #[test]
    fn model_show_names_the_model_effort_and_what_runs() {
        let mut t = ThreadView {
            name: "api".into(),
            model: Some("fable".into()),
            effort: Some("high".into()),
            ..Default::default()
        };
        assert_eq!(model_show(&t), "api: model fable, effort high");
        t.actual_model = Some("claude-fable-5-1".into());
        assert_eq!(
            model_show(&t),
            "api: model fable, effort high (runs claude-fable-5-1)"
        );
        t.model = Some("claude-fable-5-1".into());
        assert_eq!(model_show(&t), "api: model claude-fable-5-1, effort high");
    }

    /// The popup above the input line: arrows pick, Tab completes, Enter on a partial command
    /// completes it (and runs one without arguments), Esc closes it and keeps the line. Closed,
    /// it leaves the editor's keys alone; it never opens over the picker.
    #[test]
    fn the_slash_popup_picks_completes_and_runs() {
        let mut st = with_orch();
        for c in "/".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        let all = st.popup().unwrap();
        assert!(all.command && all.items.len() == slash::COMMANDS.len());
        assert_eq!(st.popup_selected(all.items.len()), 0);
        st.handle_key(key(KeyCode::Down));
        st.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(st.view.popup_sel, 2);
        st.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        st.handle_key(key(KeyCode::Up));
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.view.popup_sel, all.items.len() - 1, "wraps");
        // Typing filters and starts at the top again.
        st.handle_key(key(KeyCode::Char('h')));
        assert_eq!(st.view.popup_sel, 0);
        let items: Vec<String> = st
            .popup()
            .unwrap()
            .items
            .into_iter()
            .map(|i| i.value)
            .collect();
        assert_eq!(
            items,
            ["/help", "/orch", "/trash", "/attach"],
            "/orch: its alias has an h"
        );
        // Enter on a partial command without arguments completes and runs it.
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(st.view.help && st.view.input.is_empty());
        st.handle_key(key(KeyCode::Char('z')));
        // With arguments: completed, not run; then the threads.
        type_chars(&mut st, "/op");
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.input, "/open ");
        assert_eq!(st.view.dive, None);
        let names: Vec<String> = st
            .popup()
            .unwrap()
            .items
            .into_iter()
            .map(|i| i.value)
            .collect();
        assert_eq!(names, ["alpha", "beta", "gamma"]);
        type_chars(&mut st, "ga");
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.view.input, "/open gamma ");
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
        // In a dive, /mode suggests modes first.
        type_chars(&mut st, "/mode a");
        assert_eq!(st.popup().unwrap().items[0].value, "acceptEdits");
        // Esc closes the popup and keeps the line (and the dive).
        st.handle_key(key(KeyCode::Esc));
        assert!(st.popup().is_none());
        assert_eq!(st.view.input, "/mode a");
        assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
        // Closed, Up and Down are not the popup's; a change reopens it.
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.view.input, "/mode a");
        st.handle_key(key(KeyCode::Char('u')));
        assert!(st.popup().is_some());
        // Not over the picker, and not for a message line.
        st.view.input.clear();
        type_chars(&mut st, "hello /x");
        assert!(st.popup().is_none());
        st.view.input.set("/");
        st.view.picker = Picker::from_show(&first_show());
        assert!(st.popup().is_none());
    }

    /// `@` at the start of a word opens the popup on the worker threads, live first and never
    /// an orchestrator (current or retired). Enter or Tab puts `@name ` in place of the `@` and
    /// the rest of its word (matched on what is before the cursor) and sends nothing, except
    /// that Enter on a whole name at the end of the line sends it; Esc closes it and keeps the
    /// line. With nothing matching there is no popup and Enter sends.
    #[test]
    fn the_mention_popup_inserts_a_thread_name() {
        let mut st = with_orch();
        // A second orchestrator retires `orch`; `orbit-webapp-1` (alias `ow`) is stopped.
        let mut o = ThreadScript::new("t-orch2");
        o.at(50);
        let mut w = ThreadScript::new("t-web");
        w.at(60);
        let mut evs = orch_events(&mut o, "t-orch2");
        evs.extend([
            w.created_with_aliases("orbit-webapp-1", &["ow"]),
            w.stopped(),
        ]);
        control(&mut st, &evs);
        assert!(st.registry.get("t-orch").unwrap().retired);
        let names = |st: &AppState| -> Vec<String> {
            st.popup()
                .map(|s| s.items.into_iter().map(|i| i.value).collect())
                .unwrap_or_default()
        };
        type_chars(&mut st, "@");
        let s = st.popup().unwrap();
        assert!(s.mention && !s.command);
        assert_eq!(
            names(&st),
            ["@alpha", "@beta", "@gamma", "@orbit-webapp-1"],
            "live first, no orchestrator"
        );
        assert!(
            s.items[3].detail.starts_with("done · fake"),
            "{:?}",
            s.items[3]
        );
        // Enter puts the picked name in and sends nothing.
        st.handle_key(key(KeyCode::Down));
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.input, "@beta ");
        assert_eq!(st.view.input.cursor(), "@beta ".len());
        assert!(st.popup().is_none());
        // Mid-line: the word ending at the cursor, matched by a part of the name; the space
        // already after it is stepped over.
        st.view.input.set("ask @WEB to check");
        for _ in 0.." to check".len() {
            st.handle_key(key(KeyCode::Left));
        }
        assert_eq!(names(&st), ["@orbit-webapp-1"]);
        assert!(st.handle_key(key(KeyCode::Tab)).is_empty());
        assert_eq!(st.view.input, "ask @orbit-webapp-1 to check");
        assert_eq!(st.view.input.cursor(), "ask @orbit-webapp-1 ".len());
        // Inside a word: matched on what is before the cursor, the whole word replaced (Tab
        // and Enter alike), nothing of it left after the name.
        st.view.input.set("ask @gazz to check");
        for _ in 0.."zz to check".len() {
            st.handle_key(key(KeyCode::Left));
        }
        assert_eq!(names(&st), ["@gamma"]);
        assert!(st.handle_key(key(KeyCode::Tab)).is_empty());
        assert_eq!(st.view.input, "ask @gamma to check");
        assert_eq!(st.view.input.cursor(), "ask @gamma ".len());
        st.view.input.set("ask @bex\u{e9}y");
        for _ in 0.."x\u{e9}y".chars().count() {
            st.handle_key(key(KeyCode::Left));
        }
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.input, "ask @beta ");
        // A whole name with the cursor inside it, at the end of the line: Enter sends it.
        st.view.input.set("ask @gamma");
        for _ in 0.."ma".len() {
            st.handle_key(key(KeyCode::Left));
        }
        assert!(matches!(
            st.handle_key(key(KeyCode::Enter)).as_slice(),
            [Action::Say { text, .. }] if text == "ask @gamma"
        ));
        assert!(st.view.input.is_empty());
        // An alias finds the thread; its name goes in.
        st.view.input.set("hi @o");
        assert_eq!(names(&st), ["@orbit-webapp-1"]);
        st.view.input.set("hi @ow");
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.input, "hi @orbit-webapp-1 ");
        // A whole name at the end of the line (spaces after it aside): Enter sends it as typed.
        // Elsewhere in the line, or in another case, Enter still inserts.
        st.view.input.set("ask @gamma ");
        st.handle_key(key(KeyCode::Left));
        assert!(st.popup().is_some());
        assert!(matches!(
            st.handle_key(key(KeyCode::Enter)).as_slice(),
            [Action::Say { text, .. }] if text == "ask @gamma"
        ));
        assert!(st.view.input.is_empty());
        st.view.input.set("@gamma now");
        for _ in 0.." now".len() {
            st.handle_key(key(KeyCode::Left));
        }
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.input, "@gamma now");
        assert_eq!(st.view.input.cursor(), "@gamma ".len());
        st.view.input.set("@Gamma");
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.input, "@gamma ");
        // Esc closes the popup and keeps the line; then Enter sends it.
        st.view.input.set("@ga");
        st.handle_key(key(KeyCode::Esc));
        assert!(st.popup().is_none());
        assert_eq!(st.view.input.as_str(), "@ga");
        assert!(matches!(
            st.handle_key(key(KeyCode::Enter)).as_slice(),
            [Action::Say { text, .. }] if text == "@ga"
        ));
        // No candidates, no popup: Enter sends. Orchestrators are never offered.
        for line in ["@orch", "@zz", "mail@al"] {
            st.view.input.set(line);
            assert!(st.popup().is_none(), "{line}");
        }
        assert!(matches!(
            st.handle_key(key(KeyCode::Enter)).as_slice(),
            [Action::Say { text, .. }] if text == "mail@al"
        ));
    }

    /// Types `text` into the input line (input mode), key by key.
    fn type_chars(st: &mut AppState, text: &str) {
        for c in text.chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
    }

    /// A middle click pastes nothing while the TUI has the mouse: the status bar says how to.
    #[test]
    fn a_middle_click_says_how_to_paste() {
        let mut st = three_threads();
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        let middle = MouseButton::Middle;
        assert!(st
            .handle_mouse(mouse(MouseEventKind::Down(middle), 1, 1), &screen)
            .is_empty());
        assert_eq!(st.view.status, None);
        assert!(st
            .handle_mouse(mouse(MouseEventKind::Up(middle), 1, 1), &screen)
            .is_empty());
        assert_eq!(st.view.status.as_deref(), Some(MIDDLE_CLICK_HINT));
        assert!(st.view.input.as_str().is_empty());
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer() {
        let mut st = three_threads();
        st.dive("t-alpha");
        let mut a = ThreadScript::new("t-alpha");
        a.at(50);
        for i in 0..40 {
            st.apply_event(&thread_item(&a.text(&format!("line-{i:02}"))));
        }
        // Wide enough for the cards beside the dive.
        draw(&mut st, 100, 30);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let up = mouse(MouseEventKind::ScrollUp, tr.area.x + 1, tr.area.y + 1);
        let down = mouse(MouseEventKind::ScrollDown, tr.area.x + 1, tr.area.y + 1);
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        st.handle_mouse(up, &screen);
        assert_eq!(st.view.scroll, WHEEL_STEP);
        st.handle_mouse(down, &screen);
        st.handle_mouse(down, &screen);
        assert_eq!(st.view.scroll, 0);
        // No further up than the first line (PgUp too).
        for _ in 0..50 {
            st.handle_mouse(up, &screen);
        }
        assert_eq!(st.view.scroll, tr.max_scroll);
        assert!(tr.max_scroll > 0);
        st.handle_key(key(KeyCode::PageUp));
        assert_eq!(st.view.scroll, tr.max_scroll);
        // Over a card: the cards would scroll, but they all fit; the selection stays.
        let card = st
            .view
            .panes
            .get(&PaneId::Thread("t-beta".into()))
            .unwrap()
            .area;
        let focus = st.focused().map(str::to_string);
        st.handle_mouse(mouse(MouseEventKind::ScrollDown, card.x, card.y), &screen);
        assert_eq!(st.focused().map(str::to_string), focus);
        assert_eq!(st.view.scroll, tr.max_scroll, "the conversation stays");
        // In the decision table, the selection.
        let mut st = with_decisions();
        open_decisions(&mut st);
        draw(&mut st, 160, 20);
        let table = st.view.panes.get(&PaneId::Decisions).unwrap().area;
        st.handle_mouse(mouse(MouseEventKind::ScrollDown, table.x, table.y), &screen);
        assert_eq!(st.decision_selected(), WHEEL_STEP);
    }

    /// Threads `a`, `b`, ... (`t-a`, ...) made in that order, each running a turn, the cards'
    /// order settled.
    fn working(names: &[&str]) -> (AppState, Vec<ThreadScript>) {
        let mut st = AppState::new();
        let mut scripts = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let mut s = ThreadScript::new(format!("t-{name}"));
            let evs = [s.at(i as i64).created(name), s.turn_started("go")];
            control(&mut st, &evs);
            scripts.push(s);
        }
        st.settle_order(Instant::now());
        (st, scripts)
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| format!("t-{n}")).collect()
    }

    /// Stable order: working threads keep their places (by creation) under a stream of
    /// events from all of them, settled or not, and one that waits on background work after
    /// its turn, or starts its follow-up turn, stays in the same group and place. The
    /// recency order (`card_order = "attention"`) moves them on every event.
    #[test]
    fn stable_cards_keep_their_place_under_a_stream_of_events() {
        let (mut st, mut s) = working(&["a", "b", "c"]);
        let t0 = Instant::now();
        let abc = ids(&["a", "b", "c"]);
        assert_eq!(st.order(), abc);
        for (k, who) in [2usize, 0, 1, 2, 1, 0, 2].into_iter().enumerate() {
            let ev = s[who].at(100 + k as i64).text("working");
            control(&mut st, &[ev]);
            assert_eq!(st.order(), abc, "event {k}");
            st.settle_order(t0 + Duration::from_secs(10 * k as u64));
            assert_eq!(st.order(), abc, "event {k}, settled");
        }
        // b yields with a task still running, then its follow-up turn starts.
        let evs = [
            s[1].at(200)
                .event(EventBody::TaskUpdated(twr_core::event::TaskUpdated {
                    task_id: "t1".into(),
                    running: true,
                    ..Default::default()
                })),
            s[1].turn_ended(TurnResult::Completed),
        ];
        control(&mut st, &evs);
        assert_eq!(
            st.registry.get("t-b").unwrap().status(),
            ThreadStatus::Background
        );
        st.settle_order(t0 + Duration::from_secs(100));
        assert_eq!(st.order(), abc, "background is the working group");
        control(&mut st, &[s[1].turn_started("follow-up")]);
        st.settle_order(t0 + Duration::from_secs(110));
        assert_eq!(st.order(), abc);
        // The recency order: the last to act first.
        st.view.card_order = CardOrder::Attention;
        st.settle_order(t0 + Duration::from_secs(120));
        assert_eq!(st.order(), ids(&["b", "c", "a"]));
        assert_eq!(st.view.shown_cards, None);
        control(&mut st, &[s[0].at(300).text("again")]);
        assert_eq!(st.order(), ids(&["a", "b", "c"]), "at once");
    }

    /// A card that comes to need you goes to the top at once, even while the cards are held
    /// (a card selected, the mouse over them); once its item is answered it stays there, at
    /// its height, until the order is applied: after the delay, once nothing holds the cards.
    #[test]
    fn a_card_that_needs_you_jumps_to_the_top_and_waits_to_go_back() {
        use crate::order::ORDER_DELAY;
        let (mut st, mut s) = working(&["a", "b", "c"]);
        let t0 = Instant::now();
        draw(&mut st, 160, 40);
        st.set_focus(Some("t-a".into()));
        let list = st.view.panes.list.unwrap();
        st.view.pointer = Some((list.area.x + 1, list.area.y + 1, t0));
        assert!(st.cards_held(t0));
        for (reason, id) in [(Permission, "p1"), (Question, "q1"), (Error, "e1")] {
            let (mut st, mut s) = (st.clone(), s.clone());
            control(&mut st, &[s[2].raise(id, reason)]);
            assert_eq!(st.order(), ids(&["c", "a", "b"]), "{reason:?}, at once");
            assert!(st.card_tall("t-c"));
        }
        control(&mut st, &[s[2].raise("p1", Permission)]);
        st.settle_order(t0);
        control(&mut st, &[s[2].resolve("p1")]);
        assert!(st.card_tall("t-c"), "the height waits with the place");
        // Long past the delay, but held: c stays on top.
        let later = t0 + ORDER_DELAY * 5;
        st.settle_order(t0 + Duration::from_millis(10));
        st.settle_order(later);
        assert_eq!(st.order(), ids(&["c", "a", "b"]));
        assert_eq!(
            st.order_deadline(later),
            None,
            "the selection holds them: no timer"
        );
        // Esc: only the mouse holds them now; its hold ends HOVER_HOLD after it moved.
        st.set_focus(None);
        assert_eq!(
            st.order_deadline(later),
            Some(t0 + crate::order::HOVER_HOLD)
        );
        st.settle_order(later);
        assert_eq!(st.order(), ids(&["c", "a", "b"]));
        // The pointer moves to the conversation: let go, the delay starts, then the move.
        st.view.pointer = Some((1, 5, later));
        st.settle_order(later);
        assert_eq!(st.order(), ids(&["c", "a", "b"]));
        assert_eq!(st.order_deadline(later), Some(later + ORDER_DELAY));
        st.settle_order(later + ORDER_DELAY);
        assert_eq!(st.order(), ids(&["a", "b", "c"]));
        assert!(!st.card_tall("t-c"));
        assert_eq!(
            st.order_deadline(later + ORDER_DELAY),
            None,
            "nothing waits"
        );
        // a and b ask questions; b's permission (more urgent) places b first at once, held.
        st.set_focus(Some("t-c".into()));
        control(
            &mut st,
            &[s[0].raise("q1", Question), s[1].raise("q2", Question)],
        );
        st.settle_order(later + ORDER_DELAY * 2);
        assert_eq!(st.order(), ids(&["a", "b", "c"]));
        control(&mut st, &[s[1].raise("p2", Permission)]);
        assert_eq!(st.order(), ids(&["b", "a", "c"]), "at once");
    }

    /// Any other move waits [`crate::order::ORDER_DELAY`]: a thread that finishes leaves the
    /// working group only then, and not while the action popup is open; with `card_order =
    /// "attention"` it moves at once.
    #[test]
    fn other_moves_wait_for_the_delay_and_for_the_cards_to_be_left_alone() {
        use crate::order::ORDER_DELAY;
        let (mut st, mut s) = working(&["a", "b", "c"]);
        let t0 = Instant::now();
        let evs = [
            s[0].at(50).turn_ended(TurnResult::Completed),
            s[0].report(Some(1), twr_core::event::Grade::Success),
        ];
        control(&mut st, &evs);
        let mut old = st.clone();
        st.settle_order(t0);
        assert_eq!(st.order(), ids(&["a", "b", "c"]), "not yet");
        assert_eq!(st.order_deadline(t0), Some(t0 + ORDER_DELAY));
        st.settle_order(t0 + ORDER_DELAY - Duration::from_millis(1));
        assert_eq!(st.order(), ids(&["a", "b", "c"]), "not yet");
        // The popup open on a card holds them.
        let mut held = st.clone();
        held.view.action = Some(ActionPopup {
            target: ActionTarget::Card("t-b".into()),
            sel: None,
            filter: String::new(),
            read: None,
            armed: None,
        });
        held.settle_order(t0 + ORDER_DELAY * 2);
        assert_eq!(held.order(), ids(&["a", "b", "c"]), "held by the popup");
        held.view.action = None;
        held.settle_order(t0 + ORDER_DELAY * 2);
        assert_eq!(
            held.order(),
            ids(&["a", "b", "c"]),
            "let go: the delay starts"
        );
        held.settle_order(t0 + ORDER_DELAY * 3);
        assert_eq!(held.order(), ids(&["b", "c", "a"]));
        st.settle_order(t0 + ORDER_DELAY);
        assert_eq!(
            st.order(),
            ids(&["b", "c", "a"]),
            "done: after the working ones"
        );
        // The recency order moves it at once (and c, which acted last, leads).
        old.view.card_order = CardOrder::Attention;
        assert_eq!(old.order(), ids(&["c", "b", "a"]));
    }

    /// While a move waits, Tab steps through the cards in the order they show, and the grid
    /// keeps a thread just parked in its place until the move is applied.
    #[test]
    fn keys_and_the_grid_follow_the_cards_as_shown() {
        use crate::order::ORDER_DELAY;
        let (mut st, mut s) = working(&["a", "b", "c"]);
        draw(&mut st, 160, 40);
        control(&mut st, &[s[0].at(50).turn_ended(TurnResult::Completed)]);
        draw(&mut st, 160, 40);
        assert_eq!(st.order(), ids(&["a", "b", "c"]), "a's move waits");
        let mut tabbed = Vec::new();
        for _ in 0..3 {
            st.handle_key(key(KeyCode::Tab));
            draw(&mut st, 160, 40);
            tabbed.extend(st.focused().map(str::to_string));
        }
        assert_eq!(tabbed, ids(&["a", "b", "c"]));

        let (mut st, mut s) = working(&["a", "b", "c"]);
        st.view.show_parked = true;
        st.handle_key(ctrl('g'));
        draw(&mut st, 160, 40);
        let t0 = Instant::now();
        control(&mut st, &[s[1].at(50).parked()]);
        st.settle_order(t0);
        assert_eq!(st.grid_order(), ids(&["a", "b", "c"]), "b's tile waits");
        st.settle_order(t0 + ORDER_DELAY);
        assert_eq!(st.grid_order(), ids(&["a", "c", "b"]));
    }

    /// A detail view on screen (an item too long for its card, the orchestrator's long
    /// confirm card) holds the cards, as last drawn: not one closed since. A workspace shown
    /// again has its cards as shown, the move that waited still waiting.
    #[test]
    fn a_drawn_detail_view_holds_the_cards_and_a_workspace_keeps_them() {
        use crate::order::ORDER_DELAY;
        let (mut st, mut s) = working(&["a", "b", "c"]);
        let t0 = Instant::now();
        control(&mut st, &[s[0].at(50).turn_ended(TurnResult::Completed)]);
        st.settle_order(t0);
        st.view.panes.card = Some(crate::select::CardWindow {
            thread: "t-o".into(),
            attn_id: "c1".into(),
            revision: 1,
            width: 40,
            rows: 50,
            complete: true,
            start: 0,
            end: 10,
            max_scroll: 40,
            page: 10,
            area: ratatui::layout::Rect::new(0, 0, 40, 10),
        });
        assert!(st.cards_held(t0));
        st.settle_order(t0 + ORDER_DELAY * 2);
        assert_eq!(st.order(), ids(&["a", "b", "c"]));
        st.view.panes.card = None;
        assert!(!st.cards_held(t0));

        let saved = st.take_view();
        let mut back = AppState::new();
        back.restore_view(saved);
        assert_eq!(back.view.shown_cards, st.view.shown_cards);
        assert!(back.view.shown_cards.as_ref().unwrap().pending.is_some());
    }

    /// A list scrolled down keeps showing first the card it showed first when a card above
    /// it comes or goes: a card that comes to need you goes to the top, above the slice.
    #[test]
    fn a_scrolled_list_of_cards_does_not_jump() {
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        let (mut st, mut s) = working(&["a", "b", "c", "d", "e", "f"]);
        draw(&mut st, 160, 24);
        let list = st.view.panes.list.unwrap();
        for _ in 0..2 {
            st.handle_mouse(
                mouse(MouseEventKind::ScrollDown, list.area.x + 1, list.area.y + 1),
                &screen,
            );
            draw(&mut st, 160, 24);
        }
        let shown = cards_drawn(&st);
        assert_eq!(shown[0], "t-c", "{shown:?}");
        control(&mut st, &[s[5].raise("p1", Permission)]);
        assert_eq!(st.order()[0], "t-f");
        draw(&mut st, 160, 24);
        assert_eq!(cards_drawn(&st)[0], "t-c", "the same first card");
        assert_eq!(st.view.panes.list.unwrap().start, 3);
        // Scrolled up to the top, the list shows its new first card.
        for _ in 0..5 {
            let list = st.view.panes.list.unwrap();
            st.handle_mouse(
                mouse(MouseEventKind::ScrollUp, list.area.x + 1, list.area.y + 1),
                &screen,
            );
            draw(&mut st, 160, 24);
        }
        assert_eq!(cards_drawn(&st)[0], "t-f");
    }

    /// Scrolled to the end of the column, a card coming above still leaves the first card
    /// first (the room below the last one empty) rather than pushing it down.
    #[test]
    fn a_list_scrolled_to_its_end_does_not_jump_either() {
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        let (mut st, mut s) = working(&["a", "b", "c", "d", "e"]);
        draw(&mut st, 160, 24);
        for _ in 0..5 {
            let list = st.view.panes.list.unwrap();
            st.handle_mouse(
                mouse(MouseEventKind::ScrollDown, list.area.x + 1, list.area.y + 1),
                &screen,
            );
            draw(&mut st, 160, 24);
        }
        let list = st.view.panes.list.unwrap();
        assert_eq!(list.start, list.max, "at the end");
        let first = cards_drawn(&st)[0].clone();
        control(&mut st, &[s[4].raise("p1", Permission)]);
        draw(&mut st, 160, 24);
        assert_eq!(cards_drawn(&st)[0], first, "{:?}", cards_drawn(&st));
        assert!(st.view.panes.list.unwrap().start > st.view.panes.list.unwrap().max);
    }

    /// While the log is replayed (a workspace shown again) the cards as shown are left as
    /// they are: its threads are not all there yet, and none of them has gone.
    #[test]
    fn a_replay_leaves_the_cards_as_shown() {
        use crate::order::ORDER_DELAY;
        let (mut st, mut s) = working(&["a", "b", "c"]);
        let t0 = Instant::now();
        control(&mut st, &[s[0].at(50).turn_ended(TurnResult::Completed)]);
        st.settle_order(t0);
        let shown = st.view.shown_cards.clone();
        let mut replaying = AppState::new();
        replaying.view.shown_cards = shown.clone();
        replaying.catch_up = Some(10);
        assert_eq!(replaying.order_deadline(t0 + ORDER_DELAY), None);
        replaying.settle_order(t0 + ORDER_DELAY * 5);
        assert_eq!(replaying.view.shown_cards, shown);
    }

    /// The cards drawn, in drawing order (top to bottom, left to right).
    fn cards_drawn(st: &AppState) -> Vec<String> {
        st.view
            .panes
            .panes
            .iter()
            .filter_map(|p| match &p.id {
                PaneId::Thread(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// The wheel over the cards scrolls them a card at a time (the grid a row of tiles), in
    /// every mode and layout: the column beside the conversation, the row above it, the grid.
    /// The selection stays where it is, and may scroll out of view; a key that selects
    /// another card shows the cards around it again. The wheel over the conversation still
    /// scrolls the conversation.
    #[test]
    fn the_wheel_scrolls_the_cards_with_any_selection_and_layout() {
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        let wheel = |st: &mut AppState, down: bool, w: u16, h: u16| {
            let list = st.view.panes.list.expect("a list");
            let kind = if down {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            };
            // Under the first card: anywhere on the list scrolls it.
            st.handle_mouse(mouse(kind, list.area.x + 1, list.area.y + 1), &screen);
            draw(st, w, h);
        };
        // (size, grid, how many cards (rows of tiles) are left out at most)
        for ((w, h), grid_view, more) in [
            ((160, 24), false, 3),
            ((80, 24), false, 3),
            ((100, 14), true, 2),
        ] {
            for mode in ["none selected", "selected", "dive"] {
                let mut st = five_threads();
                if grid_view {
                    st.handle_key(ctrl('g'));
                }
                match mode {
                    "selected" => st.set_focus(Some("t-beta".into())),
                    "dive" if grid_view => continue,
                    "dive" if w < crate::ui::SIDE_MIN_WIDTH => continue,
                    "dive" => {
                        st.dive("t-beta");
                    }
                    _ => {}
                }
                draw(&mut st, w, h);
                let focus = st.focused().map(str::to_string);
                let first = cards_drawn(&st);
                let at = format!("{w}x{h} {mode}");
                wheel(&mut st, true, w, h);
                let second = cards_drawn(&st);
                assert_ne!(second, first, "{at}");
                assert!(!second.contains(&first[0]), "{at}: {second:?}");
                assert_eq!(st.focused().map(str::to_string), focus, "{at}");
                for _ in 0..10 {
                    wheel(&mut st, true, w, h);
                }
                assert_eq!(st.view.panes.list.unwrap().start, more, "{at}");
                let order = if grid_view {
                    st.grid_order()
                } else {
                    st.order()
                };
                assert_eq!(cards_drawn(&st).last(), order.last(), "{at}: the last card");
                for _ in 0..20 {
                    wheel(&mut st, false, w, h);
                }
                assert_eq!(cards_drawn(&st), first, "{at}: back at the top");
            }
        }
        // A key that selects another card shows the cards around it.
        let mut st = five_threads();
        st.set_focus(Some("t-beta".into()));
        draw(&mut st, 160, 24);
        for _ in 0..3 {
            wheel(&mut st, true, 160, 24);
        }
        assert!(!cards_drawn(&st).contains(&"t-beta".to_string()));
        st.handle_key(key(KeyCode::Tab));
        draw(&mut st, 160, 24);
        let focus = st.focused().unwrap().to_string();
        assert!(cards_drawn(&st).contains(&focus), "{focus}");
        // A click on a card scrolled to keeps the cards where they are.
        for _ in 0..3 {
            wheel(&mut st, true, 160, 24);
        }
        let shown = cards_drawn(&st);
        let last = st
            .view
            .panes
            .get(&PaneId::Thread(shown[shown.len() - 1].clone()))
            .unwrap()
            .area;
        st.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), last.x, last.y),
            &screen,
        );
        st.handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), last.x, last.y),
            &screen,
        );
        draw(&mut st, 160, 24);
        assert_eq!(st.focused(), Some(shown[shown.len() - 1].as_str()));
        assert_eq!(cards_drawn(&st), shown);
    }

    /// The wheel's scroll goes with another card selected by key: back on the first card it
    /// shows that card, not the slice the wheel left. A click on a card keeps the cards where
    /// they are drawn, scrolled or not.
    #[test]
    fn a_key_selection_ends_the_wheel_scroll_and_a_click_keeps_the_cards() {
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        let mut st = five_threads();
        st.set_focus(Some("t-beta".into()));
        draw(&mut st, 160, 24);
        for _ in 0..3 {
            let list = st.view.panes.list.unwrap();
            st.handle_mouse(
                mouse(MouseEventKind::ScrollDown, list.area.x + 1, list.area.y + 1),
                &screen,
            );
            draw(&mut st, 160, 24);
        }
        assert!(!cards_drawn(&st).contains(&"t-beta".to_string()));
        st.handle_key(key(KeyCode::Tab));
        st.handle_key(key(KeyCode::BackTab));
        assert_eq!(st.focused(), Some("t-beta"));
        draw(&mut st, 160, 24);
        assert!(cards_drawn(&st).contains(&"t-beta".to_string()));
        // The last card selected: the cards show the end. A click on the first one drawn
        // leaves them there.
        st.set_focus(Some("t-eps".into()));
        draw(&mut st, 160, 24);
        let shown = cards_drawn(&st);
        assert!(st.view.panes.list.unwrap().start > 0, "{shown:?}");
        let first = st
            .view
            .panes
            .get(&PaneId::Thread(shown[0].clone()))
            .unwrap()
            .area;
        st.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), first.x, first.y),
            &screen,
        );
        st.handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), first.x, first.y),
            &screen,
        );
        assert_eq!(st.focused(), Some(shown[0].as_str()));
        draw(&mut st, 160, 24);
        assert_eq!(cards_drawn(&st), shown);
    }

    /// A grid scrolled by the wheel and a double click on a tile far down: the dive's column
    /// of cards shows that thread's card (a grid row is no card index).
    #[test]
    fn a_dive_from_a_scrolled_grid_shows_its_card() {
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        let mut st = five_threads();
        for i in 0..12 {
            let mut t = ThreadScript::new(format!("t-x{i:02}"));
            t.at(100 + i);
            control(
                &mut st,
                &[t.created(&format!("x{i:02}")), t.turn_started("work")],
            );
        }
        st.handle_key(ctrl('g'));
        draw(&mut st, 100, 14);
        for _ in 0..6 {
            let list = st.view.panes.list.unwrap();
            st.handle_mouse(
                mouse(MouseEventKind::ScrollDown, list.area.x + 1, list.area.y + 1),
                &screen,
            );
            draw(&mut st, 100, 14);
        }
        let start = st.view.panes.list.unwrap().start;
        assert!(start >= 3, "{start}");
        let id = cards_drawn(&st)[0].clone();
        let tile = st.view.panes.get(&PaneId::Thread(id.clone())).unwrap().area;
        for _ in 0..2 {
            st.handle_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left), tile.x, tile.y),
                &screen,
            );
            st.handle_mouse(
                mouse(MouseEventKind::Up(MouseButton::Left), tile.x, tile.y),
                &screen,
            );
        }
        assert_eq!(st.view.dive.as_deref(), Some(id.as_str()));
        // A column of a few cards: the grid's row would be a card well above the dive's.
        draw(&mut st, 160, 20);
        assert!(cards_drawn(&st).len() < 6, "{:?}", cards_drawn(&st));
        assert!(cards_drawn(&st).contains(&id), "{:?}", cards_drawn(&st));
    }

    /// A card clicked where all the cards fit, then a screen too small for them all, in
    /// another layout or the same one: the card clicked stays in view.
    #[test]
    fn a_clicked_card_stays_in_view_on_a_smaller_screen() {
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        for (w, h) in [(80, 24), (160, 24)] {
            let mut st = five_threads();
            draw(&mut st, 160, 80);
            let shown = cards_drawn(&st);
            assert_eq!(shown.len(), 5);
            let last = st
                .view
                .panes
                .get(&PaneId::Thread(shown[4].clone()))
                .unwrap()
                .area;
            st.handle_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left), last.x, last.y),
                &screen,
            );
            st.handle_mouse(
                mouse(MouseEventKind::Up(MouseButton::Left), last.x, last.y),
                &screen,
            );
            assert_eq!(st.focused(), Some(shown[4].as_str()));
            draw(&mut st, w, h);
            assert!(
                cards_drawn(&st).contains(&shown[4]),
                "{w}x{h}: {:?}",
                cards_drawn(&st)
            );
        }
    }

    /// The cards follow the selection moved by keys as little as it takes: back up within the
    /// slice shown, they stay.
    #[test]
    fn the_cards_follow_the_selection_as_little_as_it_takes() {
        let mut st = five_threads();
        st.set_focus(Some(st.order()[0].clone()));
        draw(&mut st, 160, 24);
        let top = cards_drawn(&st);
        assert!(top.len() < 5, "{top:?}");
        for _ in 0..top.len() {
            st.handle_key(shift(KeyCode::Down));
            draw(&mut st, 160, 24);
        }
        let moved = cards_drawn(&st);
        assert_ne!(moved[0], top[0], "{moved:?}");
        st.handle_key(shift(KeyCode::Up));
        draw(&mut st, 160, 24);
        assert_eq!(cards_drawn(&st), moved);
    }

    #[test]
    fn mouse_toggles_with_a_command_and_the_help_closes_on_a_click() {
        let mut st = three_threads();
        st.view.mouse = true;
        assert_eq!(type_line(&mut st, "/mouse"), [Action::Mouse(false)]);
        assert!(!st.view.mouse);
        assert_eq!(type_line(&mut st, "/mouse"), [Action::Mouse(true)]);
        assert_eq!(type_line(&mut st, "/mouse on"), [Action::Mouse(true)]);
        assert_eq!(type_line(&mut st, "/mouse off"), [Action::Mouse(false)]);
        assert!(crate::action_request(&Action::Mouse(true)).is_none());
        assert!(crate::action_request(&Action::Copy { text: "x".into() }).is_none());
        // The help: a click outside it closes it; inside, it selects.
        st.handle_key(key(KeyCode::Char('?')));
        let screen = draw(&mut st, 130, 45);
        let help = st.view.panes.get(&PaneId::Help).unwrap().area;
        let actions = drag(&mut st, &screen, (help.x, help.y), (help.x + 13, help.y));
        assert_eq!(
            actions,
            [Action::Copy {
                text: "Typed commands".into()
            }]
        );
        assert!(st.view.help);
        st.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &screen,
        );
        assert!(!st.view.help);
        assert!(st.view.selection.is_none());
    }

    /// `orchestrator.show` of a hub where no orchestrator was ever chosen.
    fn first_show() -> Value {
        serde_json::json!({
            "thread": null, "retired": 0,
            "runtime": "codex", "model": "o5",
            "configured": {"runtime": "codex", "model": "o5", "explicit": false},
            "runtimes": ["claude-code", "codex", "grok-build"],
            "installed": {"claude-code": true, "codex": false, "grok-build": true},
        })
    }

    #[test]
    fn the_picker_is_offered_only_when_nothing_was_ever_chosen() {
        let p = Picker::from_show(&first_show()).unwrap();
        assert_eq!(
            p.runtimes,
            [
                ("claude-code".to_string(), Some(true)),
                ("codex".into(), Some(false)),
                ("grok-build".into(), Some(true)),
            ]
        );
        assert_eq!((p.selected, p.default.as_str()), (1, "codex"));
        for edit in [
            |v: &mut Value| v["configured"]["explicit"] = true.into(),
            |v: &mut Value| v["thread"] = "t-orch".into(),
            |v: &mut Value| v["retired"] = 1.into(),
            // A hub that does not say: no picker.
            |v: &mut Value| {
                v["configured"].as_object_mut().unwrap().remove("explicit");
            },
            |v: &mut Value| v["runtimes"] = serde_json::json!([]),
        ] {
            let mut v = first_show();
            edit(&mut v);
            assert!(Picker::from_show(&v).is_none(), "{v}");
        }
        assert!(Picker::from_show(&Value::Null).is_none());
        // An unknown default: the first row; no `installed`: unknown.
        let mut v = first_show();
        v["configured"]["runtime"] = "fake".into();
        v.as_object_mut().unwrap().remove("installed");
        let p = Picker::from_show(&v).unwrap();
        assert_eq!(p.selected, 0);
        assert_eq!(p.runtimes[0].1, None);
    }

    #[test]
    fn the_picker_chooses_with_enter_and_waits_with_esc() {
        let mut st = AppState::new();
        st.view.picker = Picker::from_show(&first_show());
        st.begin_catch_up(5);
        assert!(!st.picker_open(), "not before the replay is applied");
        let mut st = three_threads();
        st.view.picker = Picker::from_show(&first_show());
        assert!(st.picker_open());
        // Modal: other keys (and the mouse, and pastes) do nothing but say how it works.
        assert!(st.handle_key(key(KeyCode::Char('q'))).is_empty());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("Esc to decide later"));
        st.handle_paste("hello");
        assert!(st.view.input.is_empty());
        // Up/Down and digits move; Enter chooses the highlighted one (with the configured
        // model only for the configured runtime).
        st.handle_key(key(KeyCode::Down));
        st.handle_key(key(KeyCode::Down));
        assert_eq!(st.view.picker.as_ref().unwrap().selected, 2);
        st.handle_key(key(KeyCode::Char('9')));
        assert_eq!(st.view.picker.as_ref().unwrap().selected, 2);
        st.handle_key(key(KeyCode::Char('1')));
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::UseOrchestrator {
                runtime: "claude-code".into(),
                model: None,
                summary: false,
            }]
        );
        assert!(st.view.picker.is_none() && !st.picker_open());
        let (m, p) = crate::action_request(&Action::UseOrchestrator {
            runtime: "claude-code".into(),
            model: None,
            summary: false,
        })
        .unwrap();
        assert_eq!(
            (m, p),
            (
                "orchestrator.use",
                serde_json::json!({"runtime": "claude-code", "summary": false})
            )
        );
        let mut st = three_threads();
        st.view.picker = Picker::from_show(&first_show());
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::UseOrchestrator {
                runtime: "codex".into(),
                model: Some("o5".into()),
                summary: false,
            }]
        );
        // Esc closes it without choosing; keys work as usual afterwards.
        let mut st = three_threads();
        st.view.picker = Picker::from_show(&first_show());
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(st.view.picker.is_none());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("asked again next start"));
        assert!(st.handle_key(key(KeyCode::Char('q'))).is_empty());
        assert_eq!(st.view.input, "q");
        // Ctrl-C quits from the picker.
        let mut st = three_threads();
        st.view.picker = Picker::from_show(&first_show());
        assert_eq!(st.handle_key(ctrl('c')), [Action::Quit]);
    }

    #[test]
    fn the_picker_closes_once_an_orchestrator_exists() {
        let mut st = three_threads();
        st.view.picker = Picker::from_show(&first_show());
        let mut o = ThreadScript::new("t-orch");
        o.at(60);
        let mut created = o.created("orchestrator");
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.ext = serde_json::json!({"twr": {"role": "orchestrator"}})
                .as_object()
                .cloned();
        }
        control(&mut st, &[created]);
        assert!(st.view.picker.is_none());
    }

    #[test]
    fn attention_notices_read_naturally() {
        use twr_core::event::Answer;
        let mut s = ThreadScript::new("t1");
        let mut tr = Transcript::new("t1");
        let raise = |s: &mut ThreadScript, reason, summary: &str| {
            let mut ev = s.raise("a", reason);
            if let EventBody::AttentionRaised(p) = &mut ev.body {
                p.summary = summary.into();
            }
            ev
        };
        let resolved = |by, answer: Option<&str>, ext: Option<serde_json::Value>| {
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: "a".into(),
                by,
                answer: answer.map(|a| Answer {
                    decision: a.into(),
                    ..Default::default()
                }),
                ext: ext.and_then(|v| v.as_object().cloned()),
                ..Default::default()
            })
        };
        let evs = vec![
            raise(&mut s, Error, "orchestrator crashed (exit 1): boom"),
            raise(&mut s, Permission, "Bash: rm -rf target"),
            raise(&mut s, Question, "Which file?"),
            raise(&mut s, Review, "api: turn 2 gave_up: stuck"),
            raise(&mut s, Stalled, "no events for 10m"),
            s.event(resolved(
                ResolvedBy::Timeout,
                None,
                Some(serde_json::json!({"twr": {"superseded_by": "b"}})),
            )),
            s.event(resolved(
                ResolvedBy::Timeout,
                None,
                Some(serde_json::json!({"claude": {"cancelled_by": "superseded", "superseded": true}})),
            )),
            s.event(resolved(
                ResolvedBy::Timeout,
                None,
                Some(serde_json::json!({"codex": {"cancelled_by": "runtime"}})),
            )),
            s.event(resolved(ResolvedBy::Timeout, None, None)),
            s.event(resolved(ResolvedBy::User, Some("allow"), None)),
            s.event(resolved(ResolvedBy::User, Some("cancel"), None)),
            s.event(resolved(ResolvedBy::Orchestrator, Some("deny"), None)),
            s.event(resolved(ResolvedBy::AutoRetry, None, None)),
            s.event(resolved(ResolvedBy::ThreadStopped, None, None)),
        ];
        for ev in &evs {
            tr.apply(ev);
        }
        let texts: Vec<&str> = tr
            .entries
            .iter()
            .map(|e| match e {
                Entry::Notice { text, .. } => text.as_str(),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            [
                "error: orchestrator crashed (exit 1): boom",
                "permission needed: Bash: rm -rf target",
                "question: Which file?",
                "review: api: turn 2 gave_up: stuck",
                "stalled: no events for 10m",
                "replaced by a newer error",
                "replaced by a newer request from the agent",
                "withdrawn by the agent",
                "closed without an answer",
                "answered by you: allow",
                "dismissed by you",
                "answered by the orchestrator: deny",
                "closed: twr retried the turn",
                "closed: the thread stopped",
            ]
        );
    }

    #[test]
    fn transcript_ignores_other_threads() {
        let mut st = with_orch();
        let mut other = ThreadScript::new("t-beta");
        st.apply_event(&thread_item(&other.text("not mine")));
        assert!(st.transcript.as_ref().unwrap().entries.is_empty());
    }

    /// The orchestrator, then `api` and `web` (created in that order), each waiting on a
    /// permission, api's raised first.
    fn api_and_web_waiting() -> AppState {
        let mut st = AppState::new();
        let mut o = ThreadScript::new("t-orch");
        let mut evs = orch_events(&mut o, "t-orch");
        let mut a = ThreadScript::new("t-api");
        let mut w = ThreadScript::new("t-web");
        a.at(10);
        w.at(10);
        evs.extend([a.created("api"), a.turn_started("build")]);
        evs.extend([w.created("web"), w.turn_started("deploy")]);
        a.at(20);
        let mut pa = a.raise("pa", Permission);
        if let EventBody::AttentionRaised(p) = &mut pa.body {
            p.summary = "Bash: cargo build".into();
            p.options = vec!["allow".into(), "deny".into()];
        }
        w.at(30);
        let mut pw = w.raise("pw", Permission);
        if let EventBody::AttentionRaised(p) = &mut pw.body {
            p.summary = "Bash: git push --force".into();
            p.options = vec!["allow".into(), "deny".into()];
        }
        evs.extend([pa, pw]);
        control(&mut st, &evs);
        st
    }

    fn screen_text(buf: &Buffer) -> String {
        let area = buf.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn drawn_ids(st: &AppState) -> Vec<&str> {
        st.view
            .panes
            .items
            .iter()
            .map(|d| d.attn_id.as_str())
            .collect()
    }

    /// At 100x12 with api and web both waiting, only api's card is drawn, yet Enter sent the
    /// control head as `seen_gseq`, so "allow both" let the orchestrator approve web's
    /// permission the user never saw. `seen_gseq` stops below web's raise: the hub then counts
    /// one item waiting and refuses "both".
    #[test]
    fn seen_gseq_stops_below_an_item_not_drawn() {
        let mut st = api_and_web_waiting();
        st.transcript.as_mut().unwrap().entries.push(Entry::Text {
            seq: 0,
            text: "later conversation\n".repeat(50),
            open: false,
        });
        expand_long_test_messages(&mut st);
        let api = st.attention.get("pa").unwrap().revision;
        let web = st.attention.get("pw").unwrap().revision;
        assert!(api < web && web < st.last_gseq + 1);
        let screen = draw(&mut st, 100, 12);
        assert_eq!(drawn_ids(&st), ["pa"], "{}", screen_text(&screen));
        assert_eq!(st.seen_gseq(), web - 1);
        assert_eq!(
            type_line(&mut st, "allow both"),
            [Action::Say {
                text: "allow both".into(),
                seen_gseq: web - 1,
            }]
        );
        assert!(api <= st.seen_gseq(), "api's item is still seen");
        // Both drawn (a taller terminal): the head.
        draw(&mut st, 160, 40);
        assert_eq!(drawn_ids(&st), ["pa", "pw"]);
        assert_eq!(st.seen_gseq(), st.last_gseq);
        // An item raised after the last frame was not drawn: capped below it.
        let mut d = ThreadScript::new("t-db");
        d.at(40);
        let created = d.created("db");
        control(&mut st, &[created]);
        assert_eq!(st.seen_gseq(), st.last_gseq, "no blocking item is new");
        d.at(50);
        control(&mut st, &[d.raise("pd", Permission)]);
        let db = st.attention.get("pd").unwrap().revision;
        assert_eq!(st.seen_gseq(), db - 1);
        draw(&mut st, 160, 40);
        assert_eq!(st.seen_gseq(), st.last_gseq);
    }

    /// A revision not drawn caps like an item not drawn, below the item's first raise (the
    /// hub counts it as waiting since then); non-blocking items never cap.
    #[test]
    fn seen_gseq_stops_below_a_revision_not_drawn() {
        let mut st = api_and_web_waiting();
        draw(&mut st, 160, 40);
        assert_eq!(st.seen_gseq(), st.last_gseq);
        let first = st.attention.get("pw").unwrap().first_revision;
        let mut w = ThreadScript::new("t-web");
        w.at(60);
        let mut again = w.raise("pw", Permission);
        if let EventBody::AttentionRaised(p) = &mut again.body {
            p.summary = "Bash: git push --force origin main".into();
        }
        control(&mut st, &[again]);
        assert!(st.attention.get("pw").unwrap().revision > first);
        assert_eq!(st.seen_gseq(), first - 1);
        draw(&mut st, 160, 40);
        assert_eq!(st.seen_gseq(), st.last_gseq);
        // A review (not blocking) that is not drawn changes nothing.
        let mut a = ThreadScript::new("t-api");
        a.at(70);
        control(&mut st, &[a.raise("rv", Review)]);
        assert_eq!(st.seen_gseq(), st.last_gseq);
    }

    /// A card scrolled out of the side column ("↑ N more" / "↓ N more") is not drawn: its
    /// item caps `seen_gseq`.
    #[test]
    fn seen_gseq_stops_below_a_card_scrolled_out_of_view() {
        let mut st = api_and_web_waiting();
        st.transcript.as_mut().unwrap().entries.push(Entry::Text {
            seq: 0,
            text: "later conversation\n".repeat(50),
            open: false,
        });
        let api = st.attention.get("pa").unwrap().first_revision;
        let web = st.attention.get("pw").unwrap().first_revision;
        let mut evs = Vec::new();
        for (i, name) in ["db", "ui", "cache", "queue"].iter().enumerate() {
            let mut t = ThreadScript::new(format!("t-{name}"));
            t.at(40 + i as i64);
            evs.extend([t.created(name), t.turn_started("work")]);
        }
        control(&mut st, &evs);
        // Web's card below the fold.
        let screen = screen_text(&draw(&mut st, 160, 12));
        assert!(screen.contains("↓ 5 more"), "{screen}");
        assert_eq!(drawn_ids(&st), ["pa"], "{screen}");
        assert_eq!(st.seen_gseq(), web - 1, "{screen}");
        // Both above it (the last card selected).
        let last = st.order().last().cloned();
        st.set_focus(last);
        let screen = screen_text(&draw(&mut st, 160, 12));
        assert!(screen.contains("↑"), "{screen}");
        assert!(drawn_ids(&st).is_empty(), "{screen}");
        assert_eq!(st.seen_gseq(), api - 1, "{screen}");
    }

    /// Nothing under the help overlay, the picker or the `/` popup is drawn: every blocking
    /// item caps `seen_gseq` then.
    #[test]
    fn seen_gseq_counts_nothing_under_an_overlay() {
        let mut st = api_and_web_waiting();
        let api = st.attention.get("pa").unwrap().first_revision;
        draw(&mut st, 160, 40);
        assert_eq!(st.seen_gseq(), st.last_gseq);
        st.view.help = true;
        draw(&mut st, 160, 40);
        assert!(drawn_ids(&st).is_empty());
        assert_eq!(st.seen_gseq(), api - 1);
        st.view.help = false;
        draw(&mut st, 160, 40);
        assert_eq!(st.seen_gseq(), st.last_gseq);
        // The `/` popup over the cards it covers.
        st.view.input.clear();
        st.view.input.insert("/");
        let screen = screen_text(&draw(&mut st, 100, 14));
        let drawn = drawn_ids(&st);
        assert!(drawn.len() < 2, "{screen}");
        assert!(st.seen_gseq() < st.last_gseq, "{screen}");
    }

    /// The orchestrator's banner counts as drawn only where it is drawn: in a dive (or the
    /// grid) its blocking item caps `seen_gseq`.
    #[test]
    fn seen_gseq_counts_the_banner_only_where_it_is_drawn() {
        let (mut st, _) = worker_and_orch_item();
        let op = st.attention.get("op1").unwrap().first_revision;
        draw(&mut st, 160, 40);
        assert!(drawn_ids(&st).contains(&"op1"));
        assert_eq!(st.seen_gseq(), st.last_gseq);
        st.handle_key(ctrl('g'));
        draw(&mut st, 160, 40);
        assert!(!drawn_ids(&st).contains(&"op1"));
        assert_eq!(st.seen_gseq(), op - 1);
    }

    /// `ev` as the hub stores it: payload fields over the cap moved to `store`, a preview and an
    /// `_capped` marker left.
    fn capped(ev: &Event, store: &twr_core::blob::MemoryBlobStore) -> Event {
        let mut ev = ev.clone();
        ev.body = twr_core::blob::cap_body(&ev.body, store).unwrap();
        let data = ev.body.data().unwrap();
        assert!(twr_core::blob::has_capped(&data), "{data}");
        ev
    }

    /// The raise of `attn_id` applied last, as the control stream delivered it.
    fn raise_item(st: &AppState, ev: &Event) -> StreamItem {
        StreamItem {
            stream: StreamKind::Control,
            gseq: Some(st.last_gseq),
            event: ev.clone(),
        }
    }

    fn screen_flat(buf: &Buffer) -> String {
        screen_text(buf).replace('\n', "")
    }

    /// Visibility/scroll tests use expanded filler so it still pushes earlier cards off
    /// screen now that long messages have a bounded preview by default.
    fn expand_long_test_messages(st: &mut AppState) {
        let keys = crate::ui::block_rows(st).into_iter().filter_map(|r| {
            r.2.and_then(|l| crate::ui::linked_block(&l).map(str::to_string))
        });
        st.view.long_expanded.extend(keys);
    }

    /// When the blob fetch for a capped permission failed, the TUI showed the 512-byte preview
    /// of a summary over 4 KiB and lost that it was incomplete: at 90 columns the preview fit
    /// on the card, so it counted as drawn in full, and "allow all" relayed an approval for
    /// text the user never saw. A revision known only by its preview is drawn marked as cut
    /// short, is never answered by key, and caps `seen_gseq` below its first raise until its
    /// full text is fetched (and drawn).
    #[test]
    fn a_capped_preview_is_never_drawn_in_full() {
        let store = twr_core::blob::MemoryBlobStore::new();
        let mut st = api_and_web_waiting();
        // The orchestrator's own permission, on its banner at 90 columns.
        let mut o = ThreadScript::new("t-orch");
        o.at(80);
        let full_summary = format!("Bash: {}", "echo ok; ".repeat(500));
        let mut big = o.raise("pb", Permission);
        if let EventBody::AttentionRaised(p) = &mut big.body {
            p.summary = full_summary.clone();
            p.options = vec!["allow".into(), "deny".into()];
        }
        let preview = capped(&big, &store);
        // The preview alone (were it the whole text) fits the banner: drawn in full.
        let mut plain = big.clone();
        if let (EventBody::AttentionRaised(p), EventBody::AttentionRaised(c)) =
            (&mut plain.body, &preview.body)
        {
            p.summary = c.summary.clone();
            assert!(p.summary.len() < 1024, "the preview");
        }
        control(&mut st, &[plain]);
        st.set_focus(Some("t-orch".into()));
        draw(&mut st, 90, 40);
        assert!(drawn_ids(&st).contains(&"pb"));
        // As the stream delivers it after a failed fetch: marked, never drawn in full.
        control(&mut st, &[preview]);
        let item = st.attention.get("pb").unwrap().clone();
        assert!(st.incomplete(&item).is_some());
        let screen = screen_flat(&draw(&mut st, 90, 40));
        assert!(screen.contains("(truncated:"), "{screen}");
        assert!(!drawn_ids(&st).contains(&"pb"), "{screen}");
        assert_eq!(st.seen_gseq(), item.first_revision - 1);
        assert_eq!(
            type_line(&mut st, "allow all"),
            [Action::Say {
                text: "allow all".into(),
                seen_gseq: item.first_revision - 1,
            }]
        );
        st.set_focus(Some("t-orch".into()));
        settled(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        // A worker's card the same way.
        let mut a = ThreadScript::new("t-api");
        a.at(81);
        let mut wb = a.raise("wb", Permission);
        if let EventBody::AttentionRaised(p) = &mut wb.body {
            p.summary = full_summary.clone();
        }
        control(&mut st, &[a.resolve("pa"), capped(&wb, &store)]);
        st.set_focus(Some("t-api".into()));
        draw(&mut st, 160, 40);
        assert!(!drawn_ids(&st).contains(&"wb"));
        control(&mut st, &[a.resolve("wb")]);

        // Fetching: one at a time per revision; a failure says so and waits before retrying.
        let now = Instant::now();
        let due = st.hydration_due(now);
        assert_eq!(due.len(), 1);
        assert_eq!((due[0].0.as_str(), due[0].1), ("pb", item.revision));
        assert!(st.hydration_due(now).is_empty(), "already fetching");
        st.hydrated("pb", item.revision, Err("blob.get: timeout".into()), now);
        let screen = screen_flat(&draw(&mut st, 90, 40));
        assert!(screen.contains("(could not load"), "{screen}");
        assert!(!drawn_ids(&st).contains(&"pb"));
        assert_eq!(st.seen_gseq(), item.first_revision - 1);
        assert!(st.hydration_due(now).is_empty(), "not due yet");
        let again = st.next_hydration().expect("a retry");
        assert!(again > now);
        assert_eq!(st.hydration_due(again).len(), 1);
        // A result still capped (the blob store lacks it) is a failure too.
        let still = raise_item(&st, &capped(&big, &store));
        let mut still = still;
        still.gseq = Some(item.revision);
        st.hydrated("pb", item.revision, Ok(still), again);
        assert!(st.incomplete(st.attention.get("pb").unwrap()).is_some());
        assert!(st.next_hydration().unwrap() > again);

        // The full text arrives: the same revision, now whole (too long for a card, so still
        // not drawn in full, but no longer a preview).
        let mut full = raise_item(&st, &big);
        full.gseq = Some(item.revision);
        st.hydrated("pb", item.revision, Ok(full), again);
        let now_item = st.attention.get("pb").unwrap();
        assert!(st.incomplete(now_item).is_none());
        assert_eq!(now_item.summary, full_summary);
        assert_eq!(now_item.first_revision, item.first_revision);
        let screen = screen_flat(&draw(&mut st, 90, 40));
        assert!(!screen.contains("(truncated:") && !screen.contains("(could not"));
    }

    /// A capped revision whose card would fit (only its tool input was moved to the blob
    /// store) is drawn in full once fetched, not before; a stale fetch result changes nothing.
    #[test]
    fn a_capped_revision_counts_once_its_full_text_arrives() {
        let store = twr_core::blob::MemoryBlobStore::new();
        let mut st = api_and_web_waiting();
        let mut a = ThreadScript::new("t-api");
        a.at(80);
        let mut big = a.raise("pb", Permission);
        if let EventBody::AttentionRaised(p) = &mut big.body {
            p.summary = "Write: notes.md".into();
            p.options = vec!["allow".into(), "deny".into()];
            let mut ext = twr_core::event::JsonMap::new();
            ext.insert(
                "claude".into(),
                serde_json::json!({"input": {"content": "x".repeat(6000)}}),
            );
            p.ext = Some(ext);
        }
        control(&mut st, &[a.resolve("pa"), capped(&big, &store)]);
        let item = st.attention.get("pb").unwrap().clone();
        assert_eq!(item.summary, "Write: notes.md");
        st.set_focus(Some("t-api".into()));
        draw(&mut st, 160, 40);
        assert!(!drawn_ids(&st).contains(&"pb"));
        assert_eq!(st.seen_gseq(), item.first_revision - 1);
        // Its popup draws the preview, marked as cut, never in full: nothing answers it.
        popup_open(&mut st);
        assert!(!drawn_ids(&st).contains(&"pb"));
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert_eq!(st.view.status.as_deref(), Some(NOT_ON_SCREEN));
        st.handle_key(key(KeyCode::Esc));
        // A result for another revision is ignored.
        let due = st.hydration_due(Instant::now());
        assert_eq!(due.len(), 1);
        let mut full = raise_item(&st, &big);
        full.gseq = Some(item.revision);
        st.hydrated("pb", item.revision + 1, Ok(full.clone()), Instant::now());
        assert!(st.incomplete(&item).is_some());
        st.hydrated("pb", item.revision, Ok(full), Instant::now());
        assert!(st.incomplete(st.attention.get("pb").unwrap()).is_none());
        draw(&mut st, 160, 40);
        assert!(drawn_ids(&st).contains(&"pb"));
        assert_eq!(st.seen_gseq(), st.last_gseq);
        assert!(matches!(
            answer_with(&mut st, 1).as_slice(),
            [Action::Respond { attn_id, .. }] if attn_id == "pb"
        ));
        // A capped re-raise is a new incomplete revision; an uncapped one clears it.
        a.at(90);
        control(&mut st, &[capped(&big, &store)]);
        assert!(st.incomplete(st.attention.get("pb").unwrap()).is_some());
        control(&mut st, &[a.raise("pb", Permission)]);
        assert!(st.incomplete.is_empty());
        // Resolved: forgotten.
        control(&mut st, &[capped(&big, &store)]);
        assert!(!st.incomplete.is_empty());
        control(&mut st, &[a.resolve("pb")]);
        assert!(st.incomplete.is_empty());
    }

    /// Only the first nine options were drawn, yet the card counted as drawn in full; with
    /// nine neutral options and a tenth `Yes`, "yes" reached the orchestrator with `seen_gseq`
    /// past the question, and the hidden option was relayed. Every option is drawn (`10. Yes`,
    /// no key); one that does not fit leaves the item not drawn in full.
    #[test]
    fn every_option_is_drawn_or_the_item_is_unseen() {
        let mut st = api_and_web_waiting();
        let mut a = ThreadScript::new("t-api");
        a.at(80);
        let mut q = a.raise("q10", Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = "Which color?".into();
            p.options = [
                "red", "green", "blue", "cyan", "teal", "gray", "pink", "gold", "plum", "Yes",
            ]
            .iter()
            .map(|o| o.to_string())
            .collect();
        }
        control(&mut st, &[a.resolve("pa"), q]);
        let first = st.attention.get("q10").unwrap().first_revision;
        st.set_focus(Some("t-api".into()));
        let screen = screen_flat(&draw(&mut st, 160, 40));
        assert!(screen.contains("10. Yes"), "{screen}");
        assert!(drawn_ids(&st).contains(&"q10"), "{screen}");
        assert_eq!(st.seen_gseq(), st.last_gseq);
        st.transcript.as_mut().unwrap().entries.push(Entry::Text {
            seq: 0,
            text: "later conversation\n".repeat(50),
            open: false,
        });
        // Long options: the tenth does not fit on the card, so the item is not drawn in full.
        expand_long_test_messages(&mut st);
        let mut q = a.raise("q10", Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = "Which color?".into();
            p.options = (1..=9)
                .map(|n| format!("a neutral choice number {n}"))
                .chain(["Yes".to_string()])
                .collect();
        }
        control(&mut st, &[q]);
        let screen = screen_flat(&draw(&mut st, 160, 40));
        assert!(!screen.contains("10. Yes"), "{screen}");
        assert!(!drawn_ids(&st).contains(&"q10"), "{screen}");
        assert_eq!(st.seen_gseq(), first - 1);
        assert_eq!(
            type_line(&mut st, "yes"),
            [Action::Say {
                text: "yes".into(),
                seen_gseq: first - 1,
            }]
        );
        settled(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
    }

    /// At most [`HYDRATION_PARALLEL`] fetches run at once; while that many run no retry is
    /// scheduled (the next one starts when one ends), so the IO loop never spins.
    #[test]
    fn capped_fetches_are_limited() {
        let store = twr_core::blob::MemoryBlobStore::new();
        let mut st = api_and_web_waiting();
        let mut a = ThreadScript::new("t-api");
        a.at(80);
        let n = HYDRATION_PARALLEL + 2;
        for k in 0..n {
            let mut big = a.raise(&format!("c{k}"), Permission);
            if let EventBody::AttentionRaised(p) = &mut big.body {
                p.summary = "x ".repeat(3000);
            }
            control(&mut st, &[capped(&big, &store)]);
        }
        let now = Instant::now();
        let due = st.hydration_due(now);
        assert_eq!(due.len(), HYDRATION_PARALLEL);
        assert!(st.hydration_due(now).is_empty());
        // One fails: it waits, and nothing is scheduled while the others still run.
        st.hydrated(&due[0].0, due[0].1, Err("gone".into()), now);
        assert_eq!(
            st.hydration_due(now).len(),
            1,
            "a slot is free for one due now"
        );
        assert_eq!(st.next_hydration(), None, "all slots taken");
        for (id, rev, _) in &due[1..] {
            st.hydrated(id, *rev, Err("gone".into()), now);
        }
        assert!(st.next_hydration().is_some());
    }

    /// A confirm card the hub raised (`ext.twr.proposal`): its summary lists the actions the
    /// orchestrator proposed, its options are Confirm and Cancel.
    fn confirm_card(attn_id: &str) -> Event {
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(90);
        let mut ev = h.raise(attn_id, Question);
        if let EventBody::AttentionRaised(p) = &mut ev.body {
            p.summary = "From your message: allow w (Bash: git fetch origin) · set w → auto".into();
            p.options = vec!["Confirm".into(), "Cancel".into()];
            p.ext = serde_json::json!({"twr": {"proposal": {"actions": []}}})
                .as_object()
                .cloned();
        }
        ev
    }

    /// One worker `w`, the orchestrator `t-orch`, and the confirm card `cc1`.
    fn worker_orch_and_card() -> AppState {
        let mut st = AppState::new();
        let mut w = ThreadScript::new("t-w");
        let mut evs = vec![w.created("w"), w.turn_started("work")];
        let mut o = ThreadScript::new("t-orch");
        o.at(40);
        evs.extend(orch_events(&mut o, "t-orch"));
        evs.push(confirm_card("cc1"));
        control(&mut st, &evs);
        st
    }

    /// The confirm card shows in the main conversation as the orchestrator's banner, never as
    /// the hub's card in the stack: drawn in full there (its whole action list and both
    /// options), 1 confirms it, 2 cancels it, and `x` dismisses it (a cancel), each only once
    /// it has been on screen long enough.
    #[test]
    fn a_confirm_card_is_the_banner_and_its_keys_confirm_or_cancel() {
        let mut st = worker_orch_and_card();
        assert!(st.attention.get("cc1").unwrap().proposal);
        assert!(!st.hub_asks());
        assert_eq!(st.order(), ["t-w"]);
        assert_eq!(st.orch_banner().as_deref(), Some("t-orch"));
        assert_eq!(st.stack_nav(), ["t-orch", "t-w"]);
        assert_eq!(st.top_attention("t-orch").unwrap().attn_id, "cc1");
        assert!(st.card_items(HUB_THREAD).is_empty());
        let screen = screen_flat(&draw(&mut st, 160, 40));
        assert!(screen.contains("confirm what you asked"), "{screen}");
        assert!(
            screen.contains("allow w (Bash: git fetch origin)"),
            "{screen}"
        );
        assert!(screen.contains("1 Confirm"), "{screen}");
        assert!(screen.contains("2 Cancel"), "{screen}");
        assert!(!screen.contains("twr asks"), "{screen}");
        assert_eq!(
            st.view.panes.drawn_on("t-orch").map(|d| d.attn_id.as_str()),
            Some("cc1")
        );
        assert_eq!(st.seen_gseq(), st.last_gseq);
        let rev = revision(&st, "cc1");
        st.set_focus(Some("t-orch".into()));
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        // Just drawn in the popup: not read yet.
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "cc1".into(),
                revision: rev,
                decision: "Confirm".into(),
            }]
        );
        assert_eq!(
            answer_with(&mut st, 2),
            [Action::Respond {
                attn_id: "cc1".into(),
                revision: rev,
                decision: "Cancel".into(),
            }]
        );
        assert_eq!(
            run_row(&mut st, ActionKind::Dismiss),
            [Action::Dismiss {
                attn_id: "cc1".into(),
                revision: rev,
            }]
        );
        popup_open(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('3'))).is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no option 3"));
    }

    /// The hub writes every name, label and mode id on a card losslessly, an
    /// invisible character as a visible escape and an id the runtime does not know as a custom
    /// mode. Drawn offscreen, the card proposing the id "plan" + U+200B reads differently from
    /// the one proposing "plan", in full, with both options.
    #[test]
    fn a_confirm_card_shows_an_escaped_custom_mode_as_written() {
        let screen_for = |summary: &str| {
            let mut st = AppState::new();
            let mut w = ThreadScript::new("t-w");
            let mut evs = vec![w.created("gem"), w.turn_started("work")];
            let mut o = ThreadScript::new("t-orch");
            o.at(40);
            evs.extend(orch_events(&mut o, "t-orch"));
            let mut card = confirm_card("cc1");
            if let EventBody::AttentionRaised(p) = &mut card.body {
                p.summary = summary.into();
            }
            evs.push(card);
            control(&mut st, &evs);
            screen_flat(&draw(&mut st, 220, 40))
        };
        let custom = screen_for(
            "From your message: set gem → custom mode \"plan\\u{200B}\" · answer gem: \
             \"Use\\u{200B}SQLite\" (Which store?\\u{200B})",
        );
        let plain = screen_for("From your message: set gem → plan");
        assert!(
            custom.contains("set gem → custom mode \"plan\\u{200B}\""),
            "{custom}"
        );
        assert!(
            custom.contains("answer gem: \"Use\\u{200B}SQLite\" (Which store?\\u{200B})"),
            "{custom}"
        );
        assert!(!custom.contains('\u{200B}'), "{custom}");
        assert!(plain.contains("set gem → plan"), "{plain}");
        assert!(!custom.contains("set gem → plan"), "{custom}");
        for screen in [&custom, &plain] {
            assert!(
                screen.contains("1 Confirm") && screen.contains("2 Cancel"),
                "{screen}"
            );
        }
    }

    /// A confirm card the frame cut short (a terminal too short for its options) is not seen:
    /// `seen_gseq` stays below it, and no digit answers it (with no popup open they type).
    /// Its action popup, with room for all of it, draws it in full: it is seen then, and
    /// answered once read. A dive hides the banner: its popup does not open there.
    #[test]
    fn a_confirm_card_drawn_in_part_is_answered_from_its_popup() {
        let mut st = worker_orch_and_card();
        let first = st.attention.get("cc1").unwrap().first_revision;
        st.set_focus(Some("t-orch".into()));
        // The tallest height at which the banner is on screen but not its options.
        let (h, screen) = (8..30)
            .rev()
            .map(|h| (h, screen_text(&draw(&mut st, 60, h))))
            .find(|(_, s)| s.contains("confirm what you asked") && !s.contains("2 Cancel"))
            .expect("a height that cuts the card short");
        assert!(!drawn_ids(&st).contains(&"cc1"), "{screen}");
        assert!(st.view.panes.drawn_on("t-orch").is_none(), "{screen}");
        assert_eq!(st.seen_gseq(), first - 1);
        st.note_drawn(Instant::now());
        assert_eq!(st.view.shown, None);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert_eq!(st.view.input.as_str(), "1", "typed");
        st.view.input.clear();
        st.handle_key(key(KeyCode::Enter));
        let screen = screen_text(&draw(&mut st, 60, h));
        assert!(drawn_ids(&st).contains(&"cc1"), "{screen}");
        assert_eq!(st.seen_gseq(), st.last_gseq, "{screen}");
        st.note_drawn(Instant::now());
        if let Some(s) = st.view.shown.as_mut() {
            s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
        }
        assert_eq!(st.handle_key(key(KeyCode::Char('1'))).len(), 1, "{screen}");
        // A dive hides the banner: no popup there, and it is unseen.
        st.dive("t-w");
        st.set_focus(Some("t-orch".into()));
        settled(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None);
        assert_eq!(st.seen_gseq(), first - 1);
    }

    /// A confirm card listing `n` actions, its summary built like the hub's (each item's
    /// summary cut to 60 bytes, the actions joined with ` · `).
    fn long_confirm_card(attn_id: &str, n: usize) -> Event {
        let mut ev = confirm_card(attn_id);
        let actions: Vec<String> = (0..n)
            .map(|k| match k % 3 {
                0 => format!("allow w (Bash: git fetch origin && git rebase origin/main step{k})"),
                1 => format!("deny w (Bash: rm -rf target/debug/incremental/twr-build-{k:02})"),
                _ => format!("set w{k} → auto"),
            })
            .collect();
        if let EventBody::AttentionRaised(p) = &mut ev.body {
            p.summary = format!("From your message: {}", actions.join(" · "));
        }
        ev
    }

    /// One worker `w`, the orchestrator `t-orch`, and confirm card `cc1` listing `n` actions,
    /// its banner selected.
    fn worker_orch_and_long_card(n: usize) -> AppState {
        let mut st = AppState::new();
        let mut w = ThreadScript::new("t-w");
        let mut evs = vec![w.created("w"), w.turn_started("work")];
        let mut o = ThreadScript::new("t-orch");
        o.at(40);
        evs.extend(orch_events(&mut o, "t-orch"));
        evs.push(long_confirm_card("cc1", n));
        control(&mut st, &evs);
        st.set_focus(Some("t-orch".into()));
        st
    }

    /// Draws `st` at 80 x 21 (the panes an 80 x 24 screen once had, with the input box under
    /// them), notes what it showed and pretends it has been there long enough to read. Returns
    /// the screen, flattened.
    fn settled_80x21(st: &mut AppState) -> String {
        let screen = screen_flat(&draw(st, 80, 21));
        st.note_drawn(Instant::now());
        if let Some(s) = st.view.shown.as_mut() {
            s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
        }
        screen
    }

    /// Opens the action popup (Enter), draws it at 80 x 21 settled ([`settled_80x21`]) and
    /// presses `c`; a key it refuses leaves the popup open: Esc closes it then. The screen is
    /// drawn again without the popup (what it covered is on screen again). Returns what the
    /// key did and the screen with the popup.
    fn popup_key_80x21(st: &mut AppState, c: char) -> (Vec<Action>, String) {
        st.handle_key(key(KeyCode::Enter));
        let screen = settled_80x21(st);
        let actions = st.handle_key(key(KeyCode::Char(c)));
        if actions.is_empty() {
            st.handle_key(key(KeyCode::Esc));
        }
        settled_80x21(st);
        (actions, screen)
    }

    /// At 80 x 24, with 3 and 24 actions: a confirm card too long for the banner opens in the
    /// main pane as a detail view listing every action on rows of its own, then both options;
    /// it is unseen until scrolled through to its options (PgDn, then the wheel for
    /// the 24 actions). Its action popup has room for the 3 actions: it draws the card in full
    /// and answers it. The 24 it has no room for: it says so and answers nothing until the
    /// detail view has been read to its options; then it lists them and, after the settle, 1
    /// confirms and 2 cancels. Scrolled back up, the options off screen, it still answers:
    /// every row of the detail view was on screen.
    #[test]
    fn a_confirm_card_too_long_for_the_banner_scrolls_in_the_main_pane() {
        for n in [3, 24] {
            let mut st = worker_orch_and_long_card(n);
            let first = st.attention.get("cc1").unwrap().first_revision;
            let rev = revision(&st, "cc1");
            let screen = settled_80x21(&mut st);
            let w = st.view.panes.card.clone().expect("the detail view");
            assert_eq!((w.attn_id.as_str(), w.start), ("cc1", 0), "{n}");
            assert!(w.end < w.rows, "{n}: needs scrolling");
            assert!(screen.contains("confirm what you asked"), "{n}: {screen}");
            assert!(screen.contains("PgUp/PgDn"), "{n}: {screen}");
            assert!(
                screen
                    .contains("• allow w (Bash: git fetch origin && git rebase origin/main step0)"),
                "{n}: {screen}"
            );
            assert!(!screen.contains("2 Cancel"), "{n}: {screen}");
            assert!(st.view.panes.drawn_on("t-orch").is_none(), "{n}");
            assert_eq!(st.seen_gseq(), first - 1, "{n}");
            let (actions, popup) = popup_key_80x21(&mut st, '1');
            if n == 3 {
                assert!(
                    popup.contains("1 Confirm") && popup.contains("2 Cancel"),
                    "{popup}"
                );
                assert_eq!(
                    actions,
                    [Action::Respond {
                        attn_id: "cc1".into(),
                        revision: rev,
                        decision: "Confirm".into(),
                    }]
                );
                continue;
            }
            assert!(popup.contains("too long to show here"), "{popup}");
            assert!(actions.is_empty());
            assert_eq!(st.view.status.as_deref(), Some(NOT_ON_SCREEN));
            // (The status wraps over rows the detail view gets back.)
            st.view.status = None;
            settled_80x21(&mut st);
            // Scrolled through: PgDn, and the wheel over the view for the long one.
            let mut screens = vec![screen];
            while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
                if n == 24 && screens.len() > 1 {
                    let a = st.view.panes.card.as_ref().unwrap().area;
                    let wheel = mouse(MouseEventKind::ScrollDown, a.x + 1, a.y + 1);
                    st.handle_mouse(
                        wheel,
                        &Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 24)),
                    );
                } else {
                    st.handle_key(key(KeyCode::PageDown));
                }
                screens.push(settled_80x21(&mut st));
                assert!(screens.len() < 40, "{n}: scrolling ends");
            }
            let all = screens.join("\n");
            for k in 0..n {
                assert!(
                    all.contains(&format!("step{k})"))
                        || all.contains(&format!("-{k:02})"))
                        || all.contains(&format!("w{k} → auto")),
                    "{n}: action {k} shown"
                );
            }
            let last = screens.last().unwrap();
            assert!(
                last.contains("1 Confirm") && last.contains("2 Cancel"),
                "{n}: {last}"
            );
            assert_eq!(
                st.view.panes.drawn_on("t-orch").map(|d| d.attn_id.as_str()),
                Some("cc1"),
                "{n}"
            );
            assert_eq!(st.seen_gseq(), st.last_gseq, "{n}");
            let (actions, popup) = popup_key_80x21(&mut st, '1');
            assert!(popup.contains("read in full"), "{n}: {popup}");
            assert_eq!(
                actions,
                [Action::Respond {
                    attn_id: "cc1".into(),
                    revision: rev,
                    decision: "Confirm".into(),
                }],
                "{n}"
            );
            settled_80x21(&mut st);
            assert_eq!(
                popup_key_80x21(&mut st, '2').0,
                [Action::Respond {
                    attn_id: "cc1".into(),
                    revision: rev,
                    decision: "Cancel".into(),
                }],
                "{n}"
            );
            // Back up: the options are off screen, but every row of the detail view has been
            // on screen: the popup still answers it.
            st.handle_key(key(KeyCode::PageUp));
            let up = settled_80x21(&mut st);
            assert!(!up.contains("2 Cancel"), "{n}: {up}");
            assert_eq!(popup_key_80x21(&mut st, '1').0.len(), 1, "{n}");
            // Down again: every row was on screen already, the options are again.
            while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
                st.handle_key(key(KeyCode::PageDown));
                settled_80x21(&mut st);
            }
            assert_eq!(popup_key_80x21(&mut st, '2').0.len(), 1, "{n}");
        }
    }

    /// Jumping to the end of a long card does not count rows never drawn: with rows skipped
    /// (the view put straight at its end), the card takes no key until they are shown.
    #[test]
    fn a_confirm_card_counts_only_rows_that_were_on_screen() {
        let mut st = worker_orch_and_long_card(24);
        settled_80x21(&mut st);
        let w = st.view.panes.card.clone().unwrap();
        // Straight to the end, skipping the middle rows.
        st.view.card_view.scroll = w.max_scroll;
        let screen = settled_80x21(&mut st);
        assert!(screen.contains("2 Cancel"), "{screen}");
        assert!(st.view.panes.drawn_on("t-orch").is_none());
        assert!(popup_key_80x21(&mut st, '1').0.is_empty());
        settled_80x21(&mut st);
        // PgUp/PgDn never skip a row: up to the top and back down, then it answers.
        while st.view.card_view.scroll > 0 {
            st.handle_key(key(KeyCode::PageUp));
            settled_80x21(&mut st);
        }
        while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
            st.handle_key(key(KeyCode::PageDown));
            settled_80x21(&mut st);
        }
        assert!(st.view.panes.drawn_on("t-orch").is_some());
        assert_eq!(popup_key_80x21(&mut st, '1').0.len(), 1);
    }

    /// A card of more than 65,535 rows (a label of about 1.3 MB at 20 x 24) still scrolls to
    /// its options: the detail view's scroll offsets are `usize`, never saturated at
    /// `u16::MAX`, and its popup answers it once every row has been on screen and its options
    /// are.
    #[test]
    fn a_card_of_more_rows_than_u16_still_reaches_its_options() {
        let mut st = worker_orch_and_long_card(1);
        let label = "x".repeat(1_300_000);
        let mut ev = long_confirm_card("cc1", 1);
        if let EventBody::AttentionRaised(p) = &mut ev.body {
            p.summary = format!("From your message: answer docs: \"{label}\"");
        }
        control(&mut st, &[ev]);
        // The first frame wraps the label (about a second in a debug build); later frames
        // reuse its rows ([`crate::ui::WrapCache`]) and wrap nothing.
        let settled = |st: &mut AppState| {
            let screen = screen_flat(&draw(st, 20, 24));
            st.note_drawn(Instant::now());
            if let Some(s) = st.view.shown.as_mut() {
                s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
            }
            screen
        };
        settled(&mut st);
        let wrapped = st.wraps.misses();
        let w = st.view.panes.card.clone().expect("the detail view");
        assert!(w.rows > 66_000, "{}", w.rows);
        assert!(w.max_scroll > usize::from(u16::MAX), "{}", w.max_scroll);
        assert_eq!(st.view.shown, None);
        // Every row above the last page and a row counts as seen (paging through 68,000 rows a
        // frame at a time is too slow for a unit test); one PgDn then reaches the end.
        let near = w.max_scroll - (usize::from(w.page) - 1);
        st.view.card_view.scroll = near;
        for r in st.view.card_view.seen.iter_mut().take(near) {
            *r = true;
        }
        settled(&mut st);
        assert_eq!(st.view.shown, None);
        let mut screen = String::new();
        while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
            st.handle_key(key(KeyCode::PageDown));
            screen = settled(&mut st);
        }
        // At the end of the view as drawn last (a notice in the status bar may have taken a
        // row or more from it).
        let end = st.view.panes.card.clone().unwrap();
        assert_eq!(st.view.card_view.scroll, end.max_scroll);
        assert!(screen.contains("Cancel"), "{screen}");
        assert!(st.view.panes.drawn_on("t-orch").is_some());
        assert_eq!(st.wraps.misses(), wrapped, "later frames wrap nothing");
        // Read: its popup lists the options and answers it.
        st.handle_key(key(KeyCode::Enter));
        settled(&mut st);
        assert_eq!(st.handle_key(key(KeyCode::Char('2'))).len(), 1);
    }

    /// A confirm card's rows are wrapped once and kept ([`crate::ui::WrapCache`]): frames at
    /// the same width wrap nothing, and the rows are wrapped again for another width, a new
    /// revision and a note added in the same revision (its text changed). Rows no frame uses
    /// any more are dropped.
    #[test]
    fn a_confirm_card_is_wrapped_again_only_when_it_changes() {
        let mut st = worker_orch_and_long_card(10);
        settled_80x21(&mut st);
        let first = st.wraps.misses();
        assert!(first > 0);
        let kept = st.wraps.kept();
        let w = st.view.panes.card.clone().expect("the detail view");
        for _ in 0..3 {
            settled_80x21(&mut st);
        }
        assert_eq!(st.wraps.misses(), first, "the same width wraps nothing");
        // Another width: wrapped again, once.
        let narrow = |st: &mut AppState| {
            let screen = screen_flat(&draw(st, 60, 24));
            st.note_drawn(Instant::now());
            screen
        };
        narrow(&mut st);
        let at_60 = st.wraps.misses();
        assert!(at_60 > first);
        let w60 = st.view.panes.card.clone().expect("the detail view");
        assert!(w60.width < w.width, "{} < {}", w60.width, w.width);
        assert!(
            w60.rows > w.rows,
            "{} rows narrow, {} wide",
            w60.rows,
            w.rows
        );
        narrow(&mut st);
        assert_eq!(st.wraps.misses(), at_60);
        // The rows at 80 columns are no longer kept.
        assert_eq!(st.wraps.kept(), kept);
        // A new revision: wrapped again, and the view shows it.
        let mut again = long_confirm_card("cc1", 11);
        if let EventBody::AttentionRaised(p) = &mut again.body {
            p.summary = p.summary.replacen("allow w", "allow v", 1);
        }
        control(&mut st, &[again]);
        let rev = revision(&st, "cc1");
        assert_ne!(rev, w60.revision);
        let screen = narrow(&mut st);
        let revised = st.wraps.misses();
        assert!(revised > at_60);
        assert!(screen.contains("allow v (Bash"), "{screen}");
        let w = st.view.panes.card.clone().expect("the detail view");
        assert_eq!(w.revision, rev);
        assert!(w.rows > w60.rows, "{} rows, {} before", w.rows, w60.rows);
        narrow(&mut st);
        assert_eq!(st.wraps.misses(), revised);
        // A note in the same revision: wrapped again with the note.
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(95);
        control(&mut st, &[h.annotate("cc1", "checked the plan")]);
        assert_eq!(revision(&st, "cc1"), rev);
        narrow(&mut st);
        assert!(st.wraps.misses() > revised);
        let noted = st.view.panes.card.clone().expect("the detail view");
        assert_eq!(noted.rows, w.rows + 1);
        assert_eq!(st.wraps.kept(), kept);
    }

    /// At 80 x 24, an orchestrator note of about 1,500 bytes makes the card's options,
    /// note and "+N more" taller than the pane together. The card still answers: its option
    /// rows must be on screen now and every row must have been on screen once, but the note
    /// need not fit beside the options. Before every row was shown, and while the options are
    /// scrolled away, 1, 2 and `x` take no key.
    #[test]
    fn a_confirm_card_with_a_note_taller_than_the_pane_still_answers() {
        let mut st = worker_orch_and_long_card(3);
        let note = (0..52)
            .map(|k| format!("checked step {k:02} of the plan;"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(note.len() >= 1_500, "{}", note.len());
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(95);
        control(&mut st, &[h.annotate("cc1", &note)]);
        let rev = revision(&st, "cc1");
        // Down to the end: until every row has been on screen no key answers it, whether the
        // options are on screen or not.
        let mut saw_options = false;
        loop {
            let screen = settled_80x21(&mut st);
            saw_options |= screen.contains("2 Cancel");
            if st.view.panes.card.as_ref().is_some_and(|w| w.end == w.rows) {
                break;
            }
            assert!(st.view.panes.drawn_on("t-orch").is_none(), "{screen}");
            assert_eq!(st.view.shown, None, "{screen}");
            st.handle_key(key(KeyCode::PageDown));
        }
        assert!(saw_options, "the options were on screen on the way down");
        // At the end the note fills the pane: the options are scrolled away.
        let bottom = settled_80x21(&mut st);
        assert!(bottom.contains("of the plan;"), "{bottom}");
        assert!(!bottom.contains("2 Cancel"), "{bottom}");
        assert!(st.view.panes.drawn_on("t-orch").is_none());
        // Every row has been on screen: read in full, its popup answers it, options or not.
        assert_eq!(popup_key_80x21(&mut st, '1').0.len(), 1);
        // Back up to the options: every row has been on screen, so the card answers.
        while !settled_80x21(&mut st).contains("2 Cancel") {
            st.handle_key(key(KeyCode::PageUp));
        }
        assert_eq!(
            st.view.panes.drawn_on("t-orch").map(|d| d.attn_id.as_str()),
            Some("cc1")
        );
        assert_eq!(
            popup_key_80x21(&mut st, '1').0,
            [Action::Respond {
                attn_id: "cc1".into(),
                revision: rev,
                decision: "Confirm".into(),
            }]
        );
    }

    /// The confirm card comes before the orchestrator's own items on the banner; with no
    /// orchestrator it shows on the hub's card, and the hub's other questions stay there.
    #[test]
    fn a_confirm_card_goes_first_and_falls_back_to_the_hub_card() {
        let (mut st, _) = worker_and_orch_item();
        control(&mut st, &[confirm_card("cc1")]);
        let banner: Vec<&str> = st
            .card_items("t-orch")
            .iter()
            .map(|i| i.attn_id.as_str())
            .collect();
        assert_eq!(banner, ["cc1", "op1"]);
        // A hub question stays the hub's card.
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(95);
        control(&mut st, &[h.raise("hq1", Question)]);
        assert!(st.hub_asks());
        let hub: Vec<&str> = st
            .card_items(HUB_THREAD)
            .iter()
            .map(|i| i.attn_id.as_str())
            .collect();
        assert_eq!(hub, ["hq1"]);
        // No orchestrator: the confirm card is on the hub's card.
        let mut alone = AppState::new();
        let mut w = ThreadScript::new("t-w");
        control(&mut alone, &[w.created("w"), confirm_card("cc2")]);
        assert_eq!(alone.orch_banner(), None);
        assert!(alone.hub_asks());
        assert_eq!(alone.top_attention(HUB_THREAD).unwrap().attn_id, "cc2");
    }

    /// A twr action's thread comes from its input (`thread`, or `spawn_thread`'s `name`),
    /// else from its result (`thread_name`, then `thread`): a long prompt sorts before
    /// `thread` in the compact JSON and cuts it off the input's summary. A cut or quoted value
    /// is never taken, and other tools have none.
    #[test]
    fn a_twr_action_knows_its_thread_from_its_input_or_its_result() {
        assert_eq!(
            summary_str(r#"{"mode":"auto","thread":"web"}"#, "thread").as_deref(),
            Some("web")
        );
        assert_eq!(summary_str(r#"{"prompt":"long...","#, "thread"), None);
        assert_eq!(summary_str(r#"{"thread":"we..."#, "thread"), None);
        // Inside another string the quotes are escaped: no key.
        assert_eq!(
            summary_str(r#"{"prompt":"say \"thread\":\"x\""}"#, "thread"),
            None
        );
        assert_eq!(
            call_thread(
                "mcp__twr__spawn_thread",
                r#"{"name":"api-2","project":"api"}"#
            )
            .as_deref(),
            Some("api-2")
        );
        assert_eq!(call_thread("mcp__twr__set_mode", r#"{"name":"x"}"#), None);
        // The orchestrator's model change folds under the thread it names, like a mode change.
        assert_eq!(
            call_thread("mcp__twr__set_model", r#"{"effort":"high","thread":"web"}"#).as_deref(),
            Some("web")
        );
        assert_eq!(call_thread("Bash", r#"{"thread":"web"}"#), None);
        assert_eq!(
            result_thread(
                "set_mode",
                r#"{"mode":"auto","thread":"t-1","thread_name":"web"}"#
            )
            .as_deref(),
            Some("web")
        );
        // Control items name many threads: the call is about none of them.
        assert_eq!(
            result_thread("subscribe_control", r#"[{"kind":"x","thread":"t-1"}]"#),
            None
        );
        assert_eq!(
            call_thread("mcp__twr__read_context", r#"{"thread":"x"}"#),
            None
        );
        // A delete card is about every thread it lists, not the first one its result names.
        assert_eq!(
            result_thread(
                "delete_thread",
                r#"{"thread":"t-1","thread_name":"web","threads":[{"thread":"t-1"}]}"#
            ),
            None
        );
        // A move to another runtime is still about the one thread.
        assert_eq!(
            result_thread(
                "set_model",
                r#"{"thread_name":"web","thread":"t-1","runtime":"codex","runtime_changed":true}"#
            )
            .as_deref(),
            Some("web")
        );

        let mut t = Transcript::new("t-orch");
        let mut o = ThreadScript::new("t-orch");
        let call = |o: &mut ThreadScript, id: &str, tool: &str, input: &str| {
            o.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: id.into(),
                tool: tool.into(),
                input_summary: input.into(),
                ..Default::default()
            }))
        };
        let result = |o: &mut ThreadScript, id: &str, output: &str| {
            o.event(EventBody::AgentToolResult(AgentToolResult {
                call_id: id.into(),
                ok: true,
                output_summary: output.into(),
                ..Default::default()
            }))
        };
        let evs = [
            call(
                &mut o,
                "c1",
                "mcp__twr__send",
                r#"{"prompt":"a very long prompt..."#,
            ),
            result(
                &mut o,
                "c1",
                r#"{"pending":0,"queued":false,"thread":"t-alpha","turn":2}"#,
            ),
            call(
                &mut o,
                "c2",
                "mcp__twr__set_mode",
                r#"{"mode":"auto","thread":"beta"}"#,
            ),
            result(&mut o, "c2", r#"{"thread":"t-gamma"}"#),
            call(&mut o, "c3", "Bash", "ls"),
            result(&mut o, "c3", r#"{"thread":"t-alpha"}"#),
        ];
        for ev in &evs {
            t.apply(ev);
        }
        let threads: Vec<Option<&str>> = t
            .entries
            .iter()
            .map(|e| match e {
                Entry::Tool { thread, .. } => thread.as_deref(),
                _ => panic!("{e:?}"),
            })
            .collect();
        // A successful result's thread wins over the input's reference (which may be a prefix).
        assert_eq!(threads, [Some("t-alpha"), Some("t-gamma"), None]);
    }

    /// `spawn_thread` with no name and no prompt: the input names no thread, and the result
    /// (`{thread_name, thread}`, cut to its first 200 characters as runtimes summarize it)
    /// leads with the new thread's name; the thread object's own `id` and `name` are cut off.
    #[test]
    fn an_unnamed_spawn_knows_its_thread_from_its_result() {
        let thread = r#"{"aliases":[],"attached":null,"box_name":"local","crashed":false,"crashed_turn":null,"created_at":"2026-10-03T23:56:05.760Z","current_turn":null,"current_turn_retry_of":null,"effort":null,"git":null,"id":"01M4234H80DQW9N0Z0CXNMXA55","name":"api-3"}"#;
        let cut = |s: &str| format!("{}…", s.chars().take(200).collect::<String>());
        let mut t = Transcript::new("t-orch");
        let mut o = ThreadScript::new("t-orch");
        let evs = [
            o.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: "c1".into(),
                tool: "mcp__twr__spawn_thread".into(),
                input_summary: r#"{"project":"api"}"#.into(),
                ..Default::default()
            })),
            o.event(EventBody::AgentToolResult(AgentToolResult {
                call_id: "c1".into(),
                ok: true,
                output_summary: cut(&format!(r#"{{"thread_name":"api-3","thread":{thread}}}"#)),
                ..Default::default()
            })),
            // The bare thread object says nothing within the cut.
            o.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: "c2".into(),
                tool: "mcp__twr__spawn_thread".into(),
                input_summary: r#"{"project":"api"}"#.into(),
                ..Default::default()
            })),
            o.event(EventBody::AgentToolResult(AgentToolResult {
                call_id: "c2".into(),
                ok: true,
                output_summary: cut(thread),
                ..Default::default()
            })),
        ];
        for ev in &evs {
            t.apply(ev);
        }
        let threads: Vec<Option<&str>> = t
            .entries
            .iter()
            .map(|e| match e {
                Entry::Tool { thread, .. } => thread.as_deref(),
                _ => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(threads, [Some("api-3"), None]);
    }

    /// `set_model` with a long model id: the input summary (cut at 160 characters, its keys
    /// sorted) loses `thread` behind `effort` and `model`, and so would the result's summary
    /// (cut at 200) behind `applies`, `changed`, `effort` and `model`; the twr MCP server leads
    /// the result with `thread_name`, then `thread`, so the cut result still names the thread.
    #[test]
    fn a_set_model_with_a_long_model_knows_its_thread_from_its_cut_result() {
        let model = format!("claude-fable-5-1-{}", "x".repeat(150));
        let cut = |s: &str, n: usize| format!("{}…", s.chars().take(n).collect::<String>());
        let input = cut(
            &format!(r#"{{"effort":"xhigh","model":"{model}","thread":"orbit-webapp-1"}}"#),
            160,
        );
        let rest = format!(
            r#""applies":"after_turn","changed":true,"effort":"xhigh","model":"{model}","previous_effort":null,"previous_model":null"#
        );
        let id = "01M4234H80DQW9N0Z0CXNMXA55";
        let led = cut(
            &format!(r#"{{"thread_name":"orbit-webapp-1","thread":"{id}",{rest}}}"#),
            200,
        );
        let sorted = cut(
            &format!(r#"{{{rest},"thread":"{id}","thread_name":"orbit-webapp-1"}}"#),
            200,
        );
        let mut t = Transcript::new("t-orch");
        let mut o = ThreadScript::new("t-orch");
        let mut evs = Vec::new();
        for (id, output) in [("c1", &led), ("c2", &sorted)] {
            evs.push(o.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: id.into(),
                tool: "mcp__twr__set_model".into(),
                input_summary: input.clone(),
                ..Default::default()
            })));
            evs.push(o.event(EventBody::AgentToolResult(AgentToolResult {
                call_id: id.into(),
                ok: true,
                output_summary: output.clone(),
                ..Default::default()
            })));
        }
        for ev in &evs {
            t.apply(ev);
        }
        let threads: Vec<Option<&str>> = t
            .entries
            .iter()
            .map(|e| match e {
                Entry::Tool { thread, .. } => thread.as_deref(),
                _ => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(threads, [Some("orbit-webapp-1"), None]);
    }

    /// The input may name a thread by a prefix (`alp` for `alpha`), which the TUI does not
    /// resolve: a successful result's canonical `thread_name` wins, so a prefix call and an
    /// exact-name call about one worker group under its name. A failed call keeps its input's.
    #[test]
    fn a_successful_result_names_the_thread_over_a_prefix_in_the_input() {
        let mut t = Transcript::new("t-orch");
        let mut o = ThreadScript::new("t-orch");
        let mut evs = Vec::new();
        for (id, input, ok, output) in [
            (
                "c1",
                r#"{"thread":"alp"}"#,
                true,
                r#"{"thread_name":"alpha","thread":"t-a"}"#,
            ),
            (
                "c2",
                r#"{"thread":"alpha"}"#,
                true,
                r#"{"thread_name":"alpha","thread":"t-a"}"#,
            ),
            ("c3", r#"{"thread":"alp"}"#, false, "no thread named alp"),
        ] {
            evs.push(o.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: id.into(),
                tool: "mcp__twr__get_thread".into(),
                input_summary: input.into(),
                ..Default::default()
            })));
            evs.push(o.event(EventBody::AgentToolResult(AgentToolResult {
                call_id: id.into(),
                ok,
                output_summary: output.into(),
                ..Default::default()
            })));
        }
        for ev in &evs {
            t.apply(ev);
        }
        let threads: Vec<Option<&str>> = t
            .entries
            .iter()
            .map(|e| match e {
                Entry::Tool { thread, .. } => thread.as_deref(),
                _ => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(threads, [Some("alpha"), Some("alpha"), Some("alp")]);
    }

    /// Grok Build calls the hub's tools through its `use_tool` (tool kind `other`): the
    /// translator names the MCP tool in `ext.grok.mcp_tool` and leads the summary with it
    /// (`twr_adapters` grok tests), and gives the MCP tool's text as the result. Such a call is
    /// the twr action it runs, shown and folded like Claude Code's: its thread comes from its
    /// input, else from its result. Another MCP server's call stays as it is.
    #[test]
    fn a_grok_use_tool_call_is_the_twr_action_it_runs() {
        let grok = |mcp_tool: &str| {
            let mut ext = JsonMap::new();
            ext.insert(
                "grok".into(),
                serde_json::json!({"title": "use_tool", "tool": "use_tool", "mcp_tool": mcp_tool}),
            );
            Some(ext)
        };
        let mut t = Transcript::new("t-orch");
        let mut o = ThreadScript::new("t-orch");
        let call = |o: &mut ThreadScript, id: &str, mcp_tool: &str, input: &str| {
            o.event(EventBody::AgentToolCall(AgentToolCall {
                call_id: id.into(),
                tool: "other".into(),
                input_summary: input.into(),
                ext: grok(mcp_tool),
                ..Default::default()
            }))
        };
        let result = |o: &mut ThreadScript, id: &str, output: &str| {
            o.event(EventBody::AgentToolResult(AgentToolResult {
                call_id: id.into(),
                ok: true,
                output_summary: output.into(),
                ..Default::default()
            }))
        };
        let evs = [
            // A long prompt cuts the thread off the input: the result names it.
            call(
                &mut o,
                "c1",
                "twr__send",
                r#"twr__send {"prompt":"a very long prompt th"#,
            ),
            result(
                &mut o,
                "c1",
                r#"{"pending":0,"queued":false,"thread":"t-alpha","turn":2}"#,
            ),
            call(
                &mut o,
                "c2",
                "twr__set_mode",
                r#"twr__set_mode {"mode":"auto","thread":"t-alpha"}"#,
            ),
            result(
                &mut o,
                "c2",
                r#"{"thread":"t-alpha","thread_name":"alpha"}"#,
            ),
            call(&mut o, "c3", "twr__list_threads", "twr__list_threads"),
            call(&mut o, "c4", "pingsrv__ping", "pingsrv__ping"),
        ];
        for ev in &evs {
            t.apply(ev);
        }
        let tools: Vec<(&str, &str, Option<&str>)> = t
            .entries
            .iter()
            .map(|e| match e {
                Entry::Tool {
                    tool,
                    summary,
                    thread,
                    ..
                } => (tool.as_str(), summary.as_str(), thread.as_deref()),
                _ => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(
            tools,
            [
                (
                    "mcp__twr__send",
                    r#"{"prompt":"a very long prompt th"#,
                    Some("t-alpha")
                ),
                // The successful result's `thread_name` wins over the input's id.
                (
                    "mcp__twr__set_mode",
                    r#"{"mode":"auto","thread":"t-alpha"}"#,
                    Some("alpha")
                ),
                ("mcp__twr__list_threads", "{}", None),
                ("other", "pingsrv__ping", None),
            ]
        );
        // Not the twr server's, or a summary that does not lead with the tool: as it is.
        assert_eq!(
            twr_action("other", grok("twr2__send").as_ref(), "twr2__send"),
            None
        );
        assert_eq!(
            twr_action("other", grok("twr__send").as_ref(), "use_tool {}"),
            None
        );
        assert_eq!(twr_action("other", None, "twr__send {}"), None);
    }

    /// A delete card (`ext.twr.delete`) on the hub's thread for `threads` (id, name).
    fn delete_card_event(attn_id: &str, threads: &[(&str, &str)], summary: &str) -> Event {
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(90);
        let mut ev = h.raise(attn_id, Question);
        if let EventBody::AttentionRaised(p) = &mut ev.body {
            p.summary = summary.into();
            p.options = vec!["Delete".into(), "Cancel".into()];
            let threads: Vec<Value> = threads
                .iter()
                .map(|(id, name)| serde_json::json!({"thread": id, "name": name}))
                .collect();
            p.ext =
                serde_json::json!({"twr": {"delete": {"threads": threads, "asked_by": "user"}}})
                    .as_object()
                    .cloned();
        }
        ev
    }

    /// A delete card too big for the log keeps its names in the blob store: once its full
    /// raise is fetched, the card names every thread (not "a thread", nor only the preview's).
    #[test]
    fn a_capped_delete_card_names_its_threads_once_fetched() {
        let store = twr_core::blob::MemoryBlobStore::new();
        let mut st = with_orch();
        let threads: Vec<(String, String)> = (0..32)
            .map(|i| {
                (
                    format!("t-{i:02}"),
                    format!("{}-{i:02}", "a-thread-with-a-long-name".repeat(5)),
                )
            })
            .collect();
        let pairs: Vec<(&str, &str)> = threads
            .iter()
            .map(|(id, n)| (id.as_str(), n.as_str()))
            .collect();
        // A short summary: the names, in `ext`, are what goes to the blob store.
        let summary = "Delete 32 threads? This cannot be undone.";
        let full = delete_card_event("dc1", &pairs, summary);
        control(&mut st, &[capped(&full, &store)]);
        let want: Vec<String> = threads.iter().map(|(_, n)| n.clone()).collect();
        assert_ne!(
            st.delete_card("dc1"),
            Some(&want[..]),
            "capped: not all known"
        );
        let item = st.attention.get("dc1").unwrap().clone();
        let now = Instant::now();
        assert_eq!(st.hydration_due(now).len(), 1);
        let mut whole = raise_item(&st, &full);
        whole.gseq = Some(item.revision);
        st.hydrated("dc1", item.revision, Ok(whole), now);
        assert_eq!(st.delete_card("dc1"), Some(&want[..]));
    }

    /// A delete card names every thread it would delete: the orchestrator banner's title says
    /// ` confirm deleting alpha, gamma `, also when the card is too long for the banner and
    /// scrolls in the main pane. Resolved, it is forgotten.
    #[test]
    fn a_delete_card_titles_the_banner_with_its_threads() {
        let mut st = with_orch();
        control(
            &mut st,
            &[delete_card_event(
                "dc1",
                &[("t-alpha", "alpha"), ("t-gamma", "gamma")],
                "Delete 2 threads? This cannot be undone.",
            )],
        );
        assert_eq!(
            st.delete_card("dc1"),
            Some(&["alpha".to_string(), "gamma".to_string()][..])
        );
        assert_eq!(st.top_attention("t-orch").unwrap().attn_id, "dc1");
        let screen = screen_flat(&draw(&mut st, 160, 40));
        assert!(
            screen.contains(" confirm deleting alpha, gamma "),
            "{screen}"
        );
        assert!(!screen.contains("confirm what you asked"), "{screen}");
        // Too long for the banner: the detail view has the same title.
        let long: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        control(
            &mut st,
            &[delete_card_event(
                "dc2",
                &[("t-beta", "beta")],
                &format!("Delete beta? · {}", long.join(" · ")),
            )],
        );
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(95);
        control(&mut st, &[h.resolve("dc1")]);
        assert_eq!(st.delete_card("dc1"), None);
        let screen = screen_flat(&draw(&mut st, 80, 21));
        assert!(st.view.panes.card.is_some(), "the detail view: {screen}");
        assert!(screen.contains(" confirm deleting beta "), "{screen}");
        // A proposal keeps its title.
        control(&mut st, &[h.resolve("dc2"), confirm_card("cc1")]);
        let screen = screen_flat(&draw(&mut st, 160, 40));
        assert!(screen.contains("confirm what you asked"), "{screen}");
    }

    /// A delete card for several threads says what goes with each on a line of its own (the
    /// hub's summary: a sentence, then one `- <name> ...` line per thread), in the banner and
    /// in the detail view alike: no line runs into the one before it.
    #[test]
    fn a_multi_thread_delete_card_starts_each_thread_on_its_own_row() {
        let line = |name: &str| {
            format!(
                "- {name} (project p): stops its runtime, removes its worktree /w/{name} (no \
                 uncommitted changes), deletes its branch twr/{name} with 1 commit on no other \
                 branch, tag or remote (not pushed)"
            )
        };
        let summary = |names: &[&str]| {
            let mut s = format!("Delete {} threads? This cannot be undone.", names.len());
            for n in names {
                s.push('\n');
                s.push_str(&line(n));
            }
            s
        };
        // The text of each row's leftmost box (the banner or the detail view).
        let rows = |buf: &Buffer| -> Vec<String> {
            screen_text(buf)
                .lines()
                .filter_map(|r| {
                    r.split(['│', '┃'])
                        .map(str::trim)
                        .find(|seg| !seg.is_empty())
                        .map(str::to_string)
                })
                .collect()
        };
        let mut st = with_orch();
        control(
            &mut st,
            &[delete_card_event(
                "dc1",
                &[("t-alpha", "alpha"), ("t-gamma", "gamma")],
                &summary(&["alpha", "gamma"]),
            )],
        );
        let screen = rows(&draw(&mut st, 160, 40));
        assert!(st.view.panes.card.is_none(), "the banner: {screen:#?}");
        assert!(
            screen
                .iter()
                .any(|r| r.ends_with("Delete 2 threads? This cannot be undone.")),
            "{screen:#?}"
        );
        for name in ["alpha", "gamma"] {
            assert!(
                screen
                    .iter()
                    .any(|r| r.starts_with(&format!("- {name} (project p): stops its runtime"))),
                "{name}: {screen:#?}"
            );
        }
        assert!(
            !screen.iter().any(|r| r.contains("undone.-")),
            "{screen:#?}"
        );
        assert!(
            !screen.iter().any(|r| r.contains("runtime- ")),
            "{screen:#?}"
        );

        // Too long for the banner: the detail view, each thread on its own row too.
        let names = ["alpha", "beta", "gamma", "delta", "eps", "zeta"];
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(95);
        control(&mut st, &[h.resolve("dc1")]);
        control(
            &mut st,
            &[delete_card_event(
                "dc2",
                &names.map(|n| ("t-x", n)),
                &summary(&names),
            )],
        );
        let screen = rows(&draw(&mut st, 80, 24));
        assert!(st.view.panes.card.is_some(), "the detail view: {screen:#?}");
        assert!(
            screen
                .iter()
                .any(|r| r.ends_with("• Delete 6 threads? This cannot be undone.")),
            "{screen:#?}"
        );
        assert!(
            screen
                .iter()
                .any(|r| r.starts_with("- alpha (project p): stops its runtime")),
            "{screen:#?}"
        );
        assert!(
            !screen.iter().any(|r| r.contains("undone.-")),
            "{screen:#?}"
        );
    }

    /// With no orchestrator (workers made with `/new` or the CLI), a delete card is on the
    /// hub's card. A several-thread one is too long for it at 100 x 24: it opens in the main
    /// pane as the scrolling detail view, titled with its threads, while the hub's card stays
    /// in the stack. Until every row has been on screen its action popup, which has no room
    /// for it either, answers nothing; scrolled to its options, a key right after the popup
    /// draws is still refused, then 1 deletes and Dismiss dismisses. A short delete card stays
    /// on the hub's card and its popup answers it as before.
    #[test]
    fn a_delete_card_without_an_orchestrator_scrolls_in_the_main_pane() {
        let names = ["alpha", "beta", "gamma", "delta", "eps"];
        let mut st = AppState::new();
        let mut evs = Vec::new();
        for n in names {
            evs.push(ThreadScript::new(format!("t-{n}")).created(n));
        }
        let mut summary = format!("Delete {} threads? This cannot be undone.", names.len());
        for n in names {
            summary.push_str(&format!(
                "\n- {n} (project p): stops its runtime, removes its worktree /w/{n} (no \
                 uncommitted changes), deletes its branch twr/{n} with 1 commit on no other \
                 branch, tag or remote (not pushed)"
            ));
        }
        let threads: Vec<(String, &str)> = names.iter().map(|n| (format!("t-{n}"), *n)).collect();
        let threads: Vec<(&str, &str)> = threads.iter().map(|(id, n)| (id.as_str(), *n)).collect();
        evs.push(delete_card_event("dc1", &threads, &summary));
        control(&mut st, &evs);
        assert!(st.orchestrator().is_none());
        assert_eq!(st.orch_banner(), None);
        assert_eq!(st.top_attention(HUB_THREAD).unwrap().attn_id, "dc1");
        st.set_focus(Some(HUB_THREAD.into()));
        let rev = revision(&st, "dc1");
        let first = st.attention.get("dc1").unwrap().first_revision;
        // Draws at 100 x 24 and notes what it showed; `settle` pretends it was there long
        // enough to read.
        let frame = |st: &mut AppState, settle: bool| {
            let screen = screen_flat(&draw(st, 100, 24));
            st.note_drawn(Instant::now());
            if let Some(s) = st.view.shown.as_mut().filter(|_| settle) {
                s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
            }
            screen
        };
        let screen = frame(&mut st, true);
        let w = st.view.panes.card.clone().expect("the detail view");
        assert_eq!(
            (w.thread.as_str(), w.attn_id.as_str(), w.start),
            (HUB_THREAD, "dc1", 0)
        );
        assert!(w.end < w.rows, "needs scrolling: {screen}");
        assert!(
            screen.contains(" confirm deleting alpha, beta, gamma, delta, eps "),
            "{screen}"
        );
        assert!(screen.contains("PgUp/PgDn"), "{screen}");
        assert!(
            screen.contains("twr asks"),
            "the hub's card stays: {screen}"
        );
        assert!(!screen.contains("2 Cancel"), "{screen}");
        assert!(st.view.panes.drawn_on(HUB_THREAD).is_none());
        assert_eq!(st.seen_gseq(), first - 1);
        for c in ['1', '2'] {
            st.handle_key(key(KeyCode::Enter));
            let popup = frame(&mut st, true);
            assert!(popup.contains("too long to show here"), "{popup}");
            assert!(st.handle_key(key(KeyCode::Char(c))).is_empty(), "{c}");
            assert_eq!(st.view.status.as_deref(), Some(NOT_ON_SCREEN), "{c}");
            st.handle_key(key(KeyCode::Esc));
        }
        frame(&mut st, false);
        // Scrolled through with PgDn: every thread's line was on screen, then the options.
        let mut screens = vec![screen];
        while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
            st.handle_key(key(KeyCode::PageDown));
            screens.push(frame(&mut st, false));
            assert!(screens.len() < 20, "scrolling ends");
        }
        let all = screens.join("\n");
        for n in names {
            assert!(
                all.contains(&format!("- {n} (project p): stops")),
                "{n}: {all}"
            );
        }
        let last = screens.last().unwrap();
        assert!(
            last.contains("1 Delete") && last.contains("2 Cancel"),
            "{last}"
        );
        assert_eq!(
            st.view
                .panes
                .drawn_on(HUB_THREAD)
                .map(|d| d.attn_id.as_str()),
            Some("dc1")
        );
        assert_eq!(st.seen_gseq(), st.last_gseq);
        // Read: its popup lists the options; just drawn, not read yet.
        st.handle_key(key(KeyCode::Enter));
        let popup = frame(&mut st, false);
        assert!(popup.contains("read in full"), "{popup}");
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        frame(&mut st, true);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "dc1".into(),
                revision: rev,
                decision: "Delete".into(),
            }]
        );
        frame(&mut st, true);
        st.handle_key(key(KeyCode::Enter));
        frame(&mut st, true);
        let target = st.view.action.as_ref().unwrap().target.clone();
        assert!(st
            .action_rows(&target)
            .iter()
            .any(|r| r.kind == ActionKind::Dismiss));
        while st.view.action.as_ref().unwrap().sel != Some(ActionKind::Dismiss) {
            st.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Dismiss {
                attn_id: "dc1".into(),
                revision: rev,
            }]
        );

        // A short delete card: on the hub's card, answered there.
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(95);
        control(
            &mut st,
            &[
                h.resolve("dc1"),
                delete_card_event("dc2", &[("t-alpha", "alpha")], "Delete alpha?"),
            ],
        );
        // The selection left the hub's card while it had nothing open.
        st.set_focus(Some(HUB_THREAD.into()));
        let rev = revision(&st, "dc2");
        let screen = frame(&mut st, true);
        assert!(st.view.panes.card.is_none(), "{screen}");
        assert!(screen.contains("twr asks"), "{screen}");
        assert!(screen.contains("1 Delete"), "{screen}");
        assert_eq!(
            st.view
                .panes
                .drawn_on(HUB_THREAD)
                .map(|d| d.attn_id.as_str()),
            Some("dc2")
        );
        assert_eq!(
            answer_with(&mut st, 1),
            [Action::Respond {
                attn_id: "dc2".into(),
                revision: rev,
                decision: "Delete".into(),
            }]
        );
    }

    /// `/delete` takes one or more threads, by any name, each once; parked or not, every one
    /// must resolve.
    #[test]
    fn slash_delete_asks_for_several_threads_at_once() {
        let mut st = with_orch();
        st.dive("t-beta");
        assert_eq!(
            type_line(&mut st, "/delete alpha t-gamma alpha"),
            [Action::Delete {
                threads: vec!["t-alpha".into(), "t-gamma".into()]
            }]
        );
        assert_eq!(
            st.view.dive, None,
            "the card shows in the main conversation"
        );
        assert_eq!(
            st.view.status.as_deref(),
            Some("delete alpha, gamma: waiting for the hub")
        );
        assert_eq!(
            lib_request(&Action::Delete {
                threads: vec!["t-alpha".into(), "t-gamma".into()]
            }),
            (
                "thread.delete",
                serde_json::json!({"threads": ["t-alpha", "t-gamma"]})
            )
        );
        // One unknown thread: nothing is asked, and the line stays.
        assert!(type_line(&mut st, "/delete alpha nope").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no thread nope"));
        assert_eq!(st.view.input.as_str(), "/delete alpha nope");
    }

    /// `/trash` asks the hub for the list, whose result opens the trash overlay until the next
    /// key; `/trash empty` asks `y` first; `/trash restore` goes straight to the hub; the
    /// results' messages go to the status bar.
    #[test]
    fn slash_trash_lists_purges_after_y_and_restores() {
        let mut st = with_orch();
        assert_eq!(type_line(&mut st, "/trash"), [Action::TrashList]);
        assert_eq!(lib_request(&Action::TrashList), ("trash.list", Value::Null));
        let list = serde_json::json!({
            "items": [{"short": "ABCDEFGH", "thread_name": "api", "project": "twr",
                       "box": "local", "deleted_at": "2026-10-04T12:30:05Z",
                       "purge_at": "2026-10-11T12:30:05Z", "size": "1.2 GB",
                       "losses": "3 uncommitted files"},
                      {"short": "IJKLMNOP", "thread_name": "web", "project": "twr",
                       "box": "local", "deleted_at": "2026-10-04T12:30:05Z",
                       "purge_at": "2026-10-11T12:30:05Z", "size": "4 KB",
                       "losses": "nothing", "restored_as": "web-2",
                       "restored_at": "2026-10-05T08:00:00Z"}],
            "message": "2 items in the trash (1.2 GB).",
        });
        st.apply_result("trash.list", &list);
        let lines: Vec<String> = crate::ui::trash_lines(&list)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            lines[1].contains("ABCDEFGH") && lines[1].contains("api"),
            "{lines:?}"
        );
        // Two lines an item: what it is, then when it goes and what it loses.
        assert!(
            lines[1].ends_with("api  twr on local · 1.2 GB"),
            "{lines:?}"
        );
        assert!(
            lines[2].ends_with(
                "deleted 2026-10-04 12:30 · purged 2026-10-11 12:30 · 3 uncommitted files"
            ),
            "{lines:?}"
        );
        assert!(
            lines[4].ends_with("nothing (restored as web-2 on 2026-10-05 08:00)"),
            "{lines:?}"
        );
        assert!(st.view.trash.is_some());
        let buf = crate::ui::draw_offscreen(&mut st, 140, 30, Instant::now());
        let shown: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(shown.contains("twr trash") && shown.contains("2 items in the trash"));
        // Taller than the screen, it scrolls: the arrows and PgUp/PgDn move it, any other key
        // closes it.
        let buf = crate::ui::draw_offscreen(&mut st, 60, 8, Instant::now());
        let shown: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(shown.contains("PgUp/PgDn scroll"), "{shown}");
        st.handle_key(key(KeyCode::Down));
        assert_eq!((st.view.trash_scroll, st.view.trash.is_some()), (1, true));
        // Never past its end: Up moves at once from the last rows.
        let max = st.view.panes.trash_max;
        for _ in 0..5 {
            st.handle_key(key(KeyCode::PageDown));
        }
        assert_eq!(st.view.trash_scroll, max);
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.view.trash_scroll, max.saturating_sub(1));
        // On a narrow screen the rows wrap inside the overlay: its right border is on every
        // row, nothing runs past it.
        let buf = crate::ui::draw_offscreen(&mut st, 60, 30, Instant::now());
        let rows: Vec<String> = (0..30)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("3 uncommitted")),
            "{rows:#?}"
        );
        for r in rows.iter().filter(|r| r.starts_with('│')) {
            assert!(r.ends_with('│'), "{r}");
        }
        // Any key closes it, and does nothing else.
        assert!(st.handle_key(key(KeyCode::Char('y'))).is_empty());
        assert!(st.view.trash.is_none());

        // `/trash empty` asks the hub which items it names first; nothing is asked yet.
        assert_eq!(
            type_line(&mut st, "/trash empty ABCDEFGH api"),
            [Action::TrashChoose {
                items: vec!["ABCDEFGH".into(), "api".into()],
                all: false
            }]
        );
        assert!(st.view.confirm.is_none());
        assert_eq!(
            lib_request(&Action::TrashChoose {
                items: Vec::new(),
                all: true
            }),
            (
                "trash.empty",
                serde_json::json!({"items": [], "all": true, "dry_run": true})
            )
        );
        let chosen = serde_json::json!({
            "dry_run": true,
            "items": [{"item": "01J000000000000000ABCDEFGH", "short": "ABCDEFGH",
                       "thread_name": "api", "size": "1.2 GB"},
                      {"item": "01J000000000000000QRSTUVWX", "short": "QRSTUVWX",
                       "thread_name": "api", "size": "4 KB"}],
            "size": "1.2 GB",
            "message": "2 items to purge (1.2 GB).",
        });
        assert!(crate::result_status("trash.empty", &chosen).is_none());
        st.apply_result("trash.empty", &chosen);
        let prompt = &st.view.confirm.as_ref().unwrap().prompt;
        assert!(
            prompt.starts_with("purge 2 items, 1.2 GB: ABCDEFGH (api), QRSTUVWX (api) for good"),
            "{prompt}"
        );
        // `y` purges exactly the ids listed, never `all` or a name again.
        let purge = st.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            purge,
            [Action::TrashEmpty {
                items: vec![
                    "01J000000000000000ABCDEFGH".into(),
                    "01J000000000000000QRSTUVWX".into()
                ]
            }]
        );
        assert_eq!(
            lib_request(&purge[0]),
            (
                "trash.empty",
                serde_json::json!({"items": ["01J000000000000000ABCDEFGH",
                                             "01J000000000000000QRSTUVWX"], "all": false})
            )
        );
        // Another key cancels.
        assert_eq!(
            type_line(&mut st, "/trash empty --all"),
            [Action::TrashChoose {
                items: Vec::new(),
                all: true
            }]
        );
        st.apply_result("trash.empty", &chosen);
        assert!(st.handle_key(key(KeyCode::Char('n'))).is_empty());
        assert!(st.view.confirm.is_none());
        // Nothing to purge: said, nothing asked.
        st.apply_result(
            "trash.empty",
            &serde_json::json!({"dry_run": true, "items": [],
                                "message": "The trash is empty: nothing to purge."}),
        );
        assert!(st.view.confirm.is_none());
        assert_eq!(
            st.view.status.as_deref(),
            Some("The trash is empty: nothing to purge.")
        );
        // A list arriving while another question waits for `y` does not take its place: that
        // `y` never purges.
        let stop = Confirm {
            action: Action::Stop {
                thread: "t1".into(),
            },
            prompt: "stop api? y to confirm, any other key cancels".into(),
            pending: "stop api: waiting for the hub".into(),
            alt: None,
        };
        st.view.confirm = Some(stop.clone());
        st.apply_result("trash.empty", &chosen);
        assert_eq!(st.view.confirm.as_ref(), Some(&stop));
        assert!(
            st.view
                .status
                .as_deref()
                .is_some_and(|s| s.contains("another question")),
            "{:?}",
            st.view.status
        );
        st.view.confirm = None;
        assert_eq!(
            type_line(&mut st, "/trash restore ABCDEFGH as api-2"),
            [Action::TrashRestore {
                item: "ABCDEFGH".into(),
                as_name: Some("api-2".into())
            }]
        );
        assert_eq!(
            lib_request(&Action::TrashRestore {
                item: "ABCDEFGH".into(),
                as_name: Some("api-2".into())
            }),
            (
                "trash.restore",
                serde_json::json!({"item": "ABCDEFGH", "as": "api-2"})
            )
        );
        let msg = serde_json::json!({"message": "api-2 is back (parked)"});
        assert_eq!(
            crate::result_status("trash.restore", &msg).as_deref(),
            Some("api-2 is back (parked)")
        );
        assert!(crate::result_status("trash.list", &list).is_none());
    }

    fn lib_request(a: &Action) -> (&'static str, Value) {
        crate::action_request(a).unwrap()
    }

    /// [`five_threads`] plus `zeta`, parked with nothing open.
    pub(crate) fn with_parked_zeta() -> AppState {
        let mut st = five_threads();
        let mut z = ThreadScript::new("t-zeta");
        z.at(60);
        control(&mut st, &[z.created("zeta"), z.parked()]);
        st
    }

    /// Parked threads are left out of the cards and tiles unless shown (`/parked`),
    /// pinned, or with an open item (`delta`'s review); the selection leaves a card that went.
    /// They stay resolvable: commands, completion and `@` mentions name them.
    #[test]
    fn parked_threads_are_hidden_from_the_listing_unless_shown() {
        let mut st = with_parked_zeta();
        assert!(st.registry.get("t-zeta").unwrap().parked);
        assert!(st.registry.get("t-delta").unwrap().parked);
        assert_eq!(
            st.order(),
            ["t-beta", "t-alpha", "t-delta", "t-gamma", "t-eps"],
            "delta has an open item: listed"
        );
        assert_eq!(
            st.grid_order(),
            ["t-beta", "t-alpha", "t-gamma", "t-eps", "t-delta"]
        );
        assert_eq!(st.hidden_parked(), ["zeta"]);
        // `/parked` lists them, and selects nothing it does not draw.
        type_line(&mut st, "/parked");
        assert!(st.view.show_parked);
        assert_eq!(st.order().last().map(String::as_str), Some("t-zeta"));
        assert!(st.hidden_parked().is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("parked threads listed (/parked hides them)")
        );
        st.set_focus(Some("t-zeta".into()));
        type_line(&mut st, "/parked");
        assert!(!st.view.show_parked);
        assert!(!st.order().contains(&"t-zeta".to_string()));
        assert_eq!(st.focused(), None, "a card that went is not selected");
        assert_eq!(
            st.view.status.as_deref(),
            Some("1 parked hidden (/parked lists them)")
        );
        // `/parked` sets or toggles it.
        type_line(&mut st, "/parked on");
        assert!(st.view.show_parked);
        type_line(&mut st, "/parked");
        assert!(!st.view.show_parked);
        type_line(&mut st, "/parked off");
        assert!(!st.view.show_parked);
        // A pinned parked thread stays listed.
        st.view.pins.insert("t-zeta".into());
        assert!(st.order().contains(&"t-zeta".to_string()));
        // Unpinned while selected (`/pin`), it leaves the listing and the selection.
        st.set_focus(Some("t-zeta".into()));
        type_line(&mut st, "/pin");
        assert!(!st.order().contains(&"t-zeta".to_string()));
        assert_eq!(st.focused(), None);
        // Once its item is resolved, delta goes too.
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        control(&mut st, &[d.resolve("rv2")]);
        assert_eq!(st.order(), ["t-beta", "t-alpha", "t-gamma", "t-eps"]);
        assert_eq!(st.hidden_parked(), ["delta", "zeta"]);
        // A failure on a parked thread shows it again.
        control(&mut st, &[d.raise("err1", Error)]);
        assert!(st.order().contains(&"t-delta".to_string()));
        // Still resolvable everywhere.
        assert_eq!(st.thread_arg(Some("zeta")).unwrap().id, "t-zeta");
        let pools = st.pools();
        assert!(pools.threads.iter().any(|(n, _)| n == "zeta"));
        assert!(pools.mentions.iter().any(|m| m.name == "zeta"));
        st.view.input.set("/resume ze");
        st.complete_slash();
        assert_eq!(st.view.input.as_str(), "/resume zeta ");
        assert_eq!(
            type_line(&mut st, "/delete zeta"),
            [Action::Delete {
                threads: vec!["t-zeta".into()]
            }]
        );
        type_line(&mut st, "/open zeta");
        assert_eq!(st.view.dive.as_deref(), Some("t-zeta"));
        // Dived into, its card is listed (drawn as where text goes).
        assert!(st.order().contains(&"t-zeta".to_string()));
        assert!(!st.hidden_parked().contains(&"zeta"));
    }

    /// What a message did is a transient status: it clears after [`TRANSIENT_STATUS`], or
    /// when the orchestrator's turn starts or ends or its reply arrives; never a newer status.
    #[test]
    fn a_transient_status_clears_by_time_or_when_the_orchestrator_answers() {
        let mut st = with_orch();
        let now = Instant::now();
        let sent = "sent to the orchestrator".to_string();
        // By time.
        st.set_status(sent.clone(), true, now);
        assert_eq!(st.status_deadline(), Some(now + TRANSIENT_STATUS));
        st.expire_status(now + TRANSIENT_STATUS - Duration::from_millis(1));
        assert_eq!(st.view.status.as_deref(), Some(sent.as_str()));
        st.expire_status(now + TRANSIENT_STATUS);
        assert_eq!(st.view.status, None);
        assert_eq!(st.status_deadline(), None);
        // A lasting status is not transient.
        st.set_status("say: boom".into(), false, now);
        assert_eq!(st.status_deadline(), None);
        st.expire_status(now + TRANSIENT_STATUS * 2);
        assert_eq!(st.view.status.as_deref(), Some("say: boom"));
        // A newer status is never cleared, and no timer waits for it.
        st.set_status(sent.clone(), true, now);
        st.view.status = Some("thread.park: no such thread".into());
        assert_eq!(st.status_deadline(), None);
        st.expire_status(now + TRANSIENT_STATUS);
        assert_eq!(
            st.view.status.as_deref(),
            Some("thread.park: no such thread")
        );
        // A worker's turn changes nothing; the orchestrator's turn start (control stream)
        // clears it.
        let mut o = ThreadScript::new("t-orch");
        o.at(300);
        let mut g = ThreadScript::new("t-gamma");
        g.at(300);
        st.set_status(sent.clone(), true, now);
        control(&mut st, &[g.turn_started("next")]);
        assert_eq!(st.view.status.as_deref(), Some(sent.as_str()));
        control(&mut st, &[o.turn_started("hello")]);
        assert_eq!(st.view.status, None);
        // So does its turn's end.
        st.set_status(
            "to the orchestrator: queued: it is busy and reads this next".into(),
            true,
            now,
        );
        control(&mut st, &[o.turn_ended(TurnResult::Completed)]);
        assert_eq!(st.view.status, None);
        // And its final reply on the thread stream (a streamed chunk does not).
        st.set_status(sent.clone(), true, now);
        let chunk = o.event(EventBody::AgentText(AgentText {
            text: "on ".into(),
            delta: true,
            ..Default::default()
        }));
        st.apply_event(&thread_item(&chunk));
        assert_eq!(st.view.status.as_deref(), Some(sent.as_str()));
        let reply = o.text("on it");
        st.apply_event(&thread_item(&reply));
        assert_eq!(st.view.status, None);
        // The orchestrator answering never clears a newer status.
        st.set_status(sent, true, now);
        st.view.status = Some("deleted beta".into());
        control(&mut st, &[o.turn_started("again")]);
        assert_eq!(st.view.status.as_deref(), Some("deleted beta"));
    }
    #[test]
    fn inline_requests_show_full_options_and_select_a_non_top_question() {
        let mut st = api_and_web_waiting();
        let mut api = ThreadScript::new("t-api");
        api.at(80);
        let mut q = api.raise("details", Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = "Keep the exact --force flag?".into();
            p.options = vec!["Keep --force".into(), "Cancel deployment".into()];
        }
        control(&mut st, &[q]);
        let screen = draw(&mut st, 160, 40);
        let text = screen_flat(&screen);
        assert!(text.contains("Keep the exact --force flag?"), "{text}");
        assert!(text.contains("1 Keep --force"), "{text}");
        assert!(text.contains("2 Cancel deployment"), "{text}");
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let row = pane
            .links
            .iter()
            .position(|l| l.as_deref() == Some("request:details"))
            .unwrap();
        let (x, y) = (pane.area.x + 2, pane.area.y + row as u16);
        st.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            &screen,
        );
        st.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x, y), &screen);
        assert_eq!(st.view.request.as_deref(), Some("details"));
        // Its action popup answers it, once on screen long enough.
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        settled(&mut st);
        let revision = st.attention.get("details").unwrap().revision;
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "details".into(),
                revision,
                decision: "Keep --force".into()
            }]
        );
        assert_eq!(
            st.view.dive, None,
            "selection keeps typing in the orchestrator conversation"
        );
        st.handle_key(key(KeyCode::Esc));
        assert!(st.view.request.is_none());
    }

    #[test]
    fn inline_request_revision_resolution_and_overlays_cannot_redirect_an_answer() {
        let mut st = api_and_web_waiting();
        draw(&mut st, 160, 40);
        step(&mut st, KeyCode::Down);
        let id = st.view.request.clone().unwrap();
        popup_open(&mut st);
        let mut api = ThreadScript::new(st.attention.get(&id).unwrap().thread.clone());
        api.at(90);
        let mut changed = api.raise(&id, Permission);
        if let EventBody::AttentionRaised(p) = &mut changed.body {
            p.summary = "Bash: git push --force origin main".into();
            p.options = vec!["deny".into()];
        }
        control(&mut st, &[changed]);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        settled(&mut st);
        st.view.help = true;
        drawn(&mut st);
        assert!(st.view.shown.is_none());
        st.view.help = false;
        settled(&mut st);
        assert!(
            matches!(st.handle_key(key(KeyCode::Char('1'))).as_slice(), [Action::Respond {decision, ..}] if decision == "deny")
        );
        control(&mut st, &[api.resolve(&id)]);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            st.view.action, None,
            "a resolved inline selection does not fall through to another card"
        );
        st.handle_key(ctrl('g'));
        assert!(
            st.view.request.is_none(),
            "changing views clears the inline selection"
        );
    }

    #[test]
    fn inline_long_request_requires_reading_every_page_before_answering() {
        let mut st = api_and_web_waiting();
        let mut api = ThreadScript::new("t-api");
        api.at(80);
        let mut long = api.raise("long", Question);
        if let EventBody::AttentionRaised(p) = &mut long.body {
            p.summary = (0..70)
                .map(|n| format!("exact request line {n}: keep --force and all details\n"))
                .collect();
            p.options = vec!["continue".into(), "cancel".into()];
        }
        control(&mut st, &[long]);
        draw(&mut st, 80, 21);
        st.select_request("long");
        let screen = settled_80x21(&mut st);
        assert!(screen.contains("exact request line 0"));
        assert!(st.view.panes.card.is_some());
        let (actions, popup) = popup_key_80x21(&mut st, '1');
        assert!(actions.is_empty());
        assert!(popup.contains("too long to show here"), "{popup}");
        let buffer = draw(&mut st, 80, 21);
        let area = st.view.panes.card.as_ref().unwrap().area;
        for _ in 0..2 {
            st.handle_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left), area.x, area.y),
                &buffer,
            );
            st.handle_mouse(
                mouse(MouseEventKind::Up(MouseButton::Left), area.x, area.y),
                &buffer,
            );
        }
        assert_eq!(st.view.request.as_deref(), Some("long"));
        assert!(st.view.dive.is_none());
        let mut screens = vec![screen];
        while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
            st.handle_key(key(KeyCode::PageDown));
            screens.push(settled_80x21(&mut st));
            assert!(screens.len() < 100);
        }
        assert!(screens.join("\n").contains("exact request line 69"));
        assert!(
            matches!(popup_key_80x21(&mut st, '2').0.as_slice(), [Action::Respond {attn_id, decision, ..}] if attn_id == "long" && decision == "cancel")
        );
        // A new wrap width invalidates the previous read history.
        draw(&mut st, 50, 21);
        st.note_drawn(Instant::now());
        st.handle_key(key(KeyCode::Enter));
        draw(&mut st, 50, 21);
        st.note_drawn(Instant::now());
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        st.handle_key(key(KeyCode::Esc));
        // Its separate worker card still supports ordinary focus and double-click dive.
        let buffer = draw(&mut st, 80, 21);
        let detail = st.view.panes.card.as_ref().unwrap().area;
        let side = st
            .view
            .panes
            .panes
            .iter()
            .find(|p| p.id == PaneId::Thread("t-api".into()) && p.area != detail)
            .unwrap()
            .area;
        for _ in 0..2 {
            st.handle_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left), side.x, side.y),
                &buffer,
            );
            st.handle_mouse(
                mouse(MouseEventKind::Up(MouseButton::Left), side.x, side.y),
                &buffer,
            );
        }
        assert!(st.view.request.is_none());
        assert_eq!(st.view.dive.as_deref(), Some("t-api"));
    }

    #[test]
    fn inline_requests_never_count_as_seen_when_clipped_or_unhydrated() {
        let store = twr_core::blob::MemoryBlobStore::new();
        let mut st = api_and_web_waiting();
        let mut api = ThreadScript::new("t-api");
        api.at(80);
        let mut long = api.raise("capped-inline", Question);
        if let EventBody::AttentionRaised(p) = &mut long.body {
            p.summary = "full command --force ".repeat(500);
            p.options = vec!["yes".into(), "no".into()];
        }
        control(&mut st, &[capped(&long, &store)]);
        draw(&mut st, 160, 40);
        st.select_request("capped-inline");
        drawn(&mut st);
        assert!(st.view.shown.is_none());
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        st.handle_key(key(KeyCode::Esc));
        assert!(!st
            .view
            .panes
            .items
            .iter()
            .any(|i| i.attn_id == "capped-inline"));
        st.view.request = None;
        draw(&mut st, 80, 7);
        st.note_drawn(Instant::now());
        assert!(
            st.view.panes.items.is_empty(),
            "clipped side and inline cards are not read"
        );
    }
    #[test]
    fn a_reused_request_id_still_shows_its_open_request() {
        // The old raise of `pa` ended; the new one is open, its update not in yet.
        let mut st = api_and_web_waiting();
        st.transcript.as_mut().unwrap().entries = vec![Entry::HubUpdate {
            text: "api needs permission: old".into(),
            user_message: None,
            attn_id: Some("pa".into()),
            about: None,
            outcome: Some("answered: allow".into()),
            seq: 3,
            resolution: false,
        }];
        let buf = draw(&mut st, 160, 40);
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap();
        let screen: String = (pane.area.y..pane.area.bottom())
            .flat_map(|y| (pane.area.x..pane.area.right()).map(move |x| (x, y)))
            .map(|p| buf[p].symbol().to_string())
            .collect();
        assert!(screen.contains("Bash: cargo build"), "{screen}");
        assert!(screen.contains("api allowed"), "{screen}");
    }

    #[test]
    fn inline_requests_keep_their_place_and_do_not_hide_later_replies() {
        let mut st = api_and_web_waiting();
        st.transcript.as_mut().unwrap().entries = vec![
            Entry::Text {
                seq: 0,
                text: "Earlier reply".into(),
                open: false,
            },
            Entry::HubUpdate {
                text: "api needs permission".into(),
                user_message: None,
                attn_id: Some("pa".into()),
                about: None,
                outcome: None,
                seq: 0,
                resolution: false,
            },
            Entry::Text {
                seq: 0,
                text: "Latest reply stays at the bottom".into(),
                open: false,
            },
        ];
        let buf = draw(&mut st, 160, 40);
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap();
        let screen = (pane.area.y..pane.area.bottom())
            .map(|y| {
                (pane.area.x..pane.area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.find("Earlier reply").unwrap() < screen.find("Bash: cargo build").unwrap(),
            "{screen}"
        );
        assert!(
            screen.find("Bash: cargo build").unwrap()
                < screen.find("Latest reply stays at the bottom").unwrap(),
            "{screen}"
        );
        assert!(pane
            .links
            .iter()
            .any(|l| l.as_deref() == Some("request:pa")));
    }

    #[test]
    fn a_selected_request_stays_visible_after_an_explanation_and_releases_on_scroll() {
        let mut st = api_and_web_waiting();
        draw(&mut st, 160, 40);
        st.select_request("pa");
        settled(&mut st);
        st.view.input.set("Explain what api is asking");
        assert!(matches!(
            st.handle_key(key(KeyCode::Enter)).as_slice(),
            [Action::Say { text, .. }] if text == "Explain what api is asking"
        ));
        st.transcript.as_mut().unwrap().entries.push(Entry::Text {
            seq: 0,
            text: "Long explanation after the selected request\n".repeat(60),
            open: false,
        });
        expand_long_test_messages(&mut st);
        settled(&mut st);
        assert_eq!(st.view.request.as_deref(), Some("pa"));
        assert!(screen_flat(&draw(&mut st, 160, 40)).contains("Long explanation"));
        assert!(matches!(
            answer_with(&mut st, 1).as_slice(),
            [Action::Respond { attn_id, .. }] if attn_id == "pa"
        ));
        let scroll = st.view.panes.get(&PaneId::Transcript).unwrap().scroll;
        assert!(scroll > 0);
        st.handle_key(key(KeyCode::PageDown));
        assert_eq!(st.view.scroll, scroll.saturating_sub(SCROLL_STEP));
        assert!(st.view.request.is_none());
        assert!(st.view.shown.is_none());
        assert!(screen_flat(&draw(&mut st, 160, 40)).contains("Long explanation"));
    }

    #[test]
    fn a_selected_worker_request_keeps_confirmations_and_orchestrator_questions_visible() {
        let mut st = api_and_web_waiting();
        draw(&mut st, 160, 40);
        st.select_request("pa");
        control(&mut st, &[confirm_card("new-confirmation")]);
        let screen = screen_flat(&draw(&mut st, 160, 40));
        assert!(
            screen.contains("1 Confirm") && screen.contains("2 Cancel"),
            "{screen}"
        );
        assert!(drawn_ids(&st).contains(&"new-confirmation"));
        assert!(drawn_ids(&st).contains(&"pa"));
        assert_eq!(st.view.request.as_deref(), Some("pa"));
        let mut hub = ThreadScript::new(HUB_THREAD);
        hub.at(100);
        let mut orch = ThreadScript::new("t-orch");
        orch.at(100);
        control(
            &mut st,
            &[
                hub.resolve("new-confirmation"),
                orch.raise("orch-question", Question),
            ],
        );
        draw(&mut st, 160, 40);
        assert!(drawn_ids(&st).contains(&"orch-question"));
        assert!(drawn_ids(&st).contains(&"pa"));
    }

    #[test]
    fn a_request_exactly_as_tall_as_the_pane_is_answerable_during_a_turn() {
        let mut st = api_and_web_waiting();
        draw(&mut st, 160, 40);
        let pane = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let mut api = ThreadScript::new("t-api");
        api.at(80);
        let mut request = api.raise("pa", Permission);
        if let EventBody::AttentionRaised(p) = &mut request.body {
            p.summary = (0..pane.height - 4)
                .map(|n| format!("command detail {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            p.options = vec!["allow".into(), "deny".into()];
        }
        let mut orch = ThreadScript::new("t-orch");
        orch.at(80);
        control(&mut st, &[request, orch.turn_started("explain")]);
        let rows = crate::ui::transcript_view(
            &[],
            pane.width,
            &crate::ui::Collapse::default(),
            &crate::ui::Threads {
                boxes: true,
                state: Some(&st),
            },
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.2.as_deref() == Some("request:pa"))
                .count(),
            usize::from(pane.height)
        );
        st.select_request("pa");
        settled(&mut st);
        assert!(st.view.panes.card.is_some());
        assert!(matches!(
            answer_with(&mut st, 1).as_slice(),
            [Action::Respond { attn_id, .. }] if attn_id == "pa"
        ));
    }

    /// With the sidebar hidden no card is selected, so Enter opens no popup until a request in
    /// the conversation is selected (Shift-↓); then its popup answers that request, after the
    /// same settle guard, and the selection (and the popup) hold while other events arrive.
    /// The popup has nothing about a thread to stop or park. Showing the sidebar again ends
    /// the selection.
    #[test]
    fn with_the_sidebar_hidden_only_a_selected_inline_request_is_answered() {
        let mut st = api_and_web_waiting();
        st.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(st.view.sidebar_hidden);
        settled(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None);
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request.as_deref(), Some("pa"));
        assert_eq!(st.focused(), None, "no card is selected");
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        assert!(
            st.handle_key(key(KeyCode::Char('1'))).is_empty(),
            "not on screen long enough to be read"
        );
        let mut w = ThreadScript::new("t-web");
        w.at(95);
        control(&mut st, &[w.turn_started("more")]);
        assert_eq!(st.view.request.as_deref(), Some("pa"), "events keep it");
        assert!(st.view.action.is_some(), "and its popup");
        settled(&mut st);
        let target = st.view.action.as_ref().unwrap().target.clone();
        assert!(st
            .action_rows(&target)
            .iter()
            .all(|r| !matches!(r.kind, ActionKind::Stop | ActionKind::Park)));
        let revision = st.attention.get("pa").unwrap().revision;
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "pa".into(),
                revision,
                decision: "allow".into()
            }]
        );
        // The orchestrator's card stays listed (and selected) with the sidebar hidden: a
        // selected request in the conversation still takes the option keys.
        let mut o = ThreadScript::new("t-orch");
        o.at(96);
        control(&mut st, &[o.raise("orch-q", Question)]);
        assert_eq!(st.nav_order(), ["t-orch"]);
        st.view.request = None;
        step(&mut st, KeyCode::Down);
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request.as_deref(), Some("pw"));
        assert_eq!(st.focused(), None);
        control(&mut st, &[w.turn_started("again")]);
        assert_eq!(st.view.request.as_deref(), Some("pw"));
        assert!(matches!(
            answer_with(&mut st, 2).as_slice(),
            [Action::Respond { attn_id, decision, .. }] if attn_id == "pw" && decision == "deny"
        ));
        st.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(!st.view.sidebar_hidden);
        assert!(st.view.request.is_none());
    }

    /// [`three_threads`] dived into `t-alpha`, its transcript `n` replies `reply(i)`; the
    /// script, for more.
    fn alpha_replies(n: usize, reply: impl Fn(usize) -> String) -> (AppState, ThreadScript) {
        let mut st = three_threads();
        st.dive("t-alpha");
        let mut a = ThreadScript::new("t-alpha");
        a.at(50);
        for i in 0..n {
            st.apply_event(&thread_item(&a.text(&reply(i))));
        }
        (st, a)
    }

    /// The transcript's rows as `buf` drew them (the pane of the last draw).
    fn transcript_rows(st: &AppState, buf: &Buffer) -> Vec<String> {
        let area = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The transcript's rows in a frame drawn `w` x `h`.
    fn drawn_rows(st: &mut AppState, w: u16, h: u16) -> Vec<String> {
        let buf = draw(st, w, h);
        transcript_rows(st, &buf)
    }

    /// A streamed piece of a reply (`agent.text`, not final).
    fn delta(a: &mut ThreadScript, text: &str) -> StreamItem {
        thread_item(&a.event(EventBody::AgentText(AgentText {
            text: text.into(),
            is_final: false,
            ..Default::default()
        })))
    }

    /// Scrolled up, the view stays on what it shows while a reply streams in below it (more
    /// rows, a reply finished, more replies), the pane's last row counting the rows below
    /// (itself one of them: it says so in their place).
    /// Scrolled back down to the bottom it follows again: new text shows as it comes.
    #[test]
    fn a_view_scrolled_up_stays_put_while_text_streams_in() {
        let (mut st, mut a) = alpha_replies(40, |i| format!("line-{i:02}"));
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::PageUp));
        let buf = draw(&mut st, 80, 30);
        let read = transcript_rows(&st, &buf);
        assert!(read.iter().any(|r| r.contains("line-29")), "{read:?}");
        assert!(!read.iter().any(|r| r.contains("line-39")), "{read:?}");
        // The working cursor's rows at the bottom go while scrolled up: 8 of the 10 rows
        // scrolled over are below, and the row that says so.
        assert_eq!(st.view.scroll, SCROLL_STEP - 2);
        assert!(screen_text(&buf).contains("↓ 9 lines below · End to follow"));
        let mut reply = String::new();
        let mut below = st.view.scroll;
        for k in 0..20 {
            let piece = format!("streamed-{k}\n\n");
            reply.push_str(&piece);
            st.apply_event(&delta(&mut a, &piece));
            let buf = draw(&mut st, 80, 30);
            assert_eq!(transcript_rows(&st, &buf), read, "after delta {k}");
            assert!(st.view.scroll > below, "{k}: {} rows below", st.view.scroll);
            below = st.view.scroll;
            let shown = format!("↓ {} lines below · End to follow", below + 1);
            assert!(screen_text(&buf).contains(&shown), "{k}: {shown}");
        }
        st.apply_event(&thread_item(&a.text(&reply)));
        for i in 0..5 {
            st.apply_event(&thread_item(&a.text(&format!("later-{i}"))));
        }
        let buf = draw(&mut st, 80, 30);
        assert_eq!(transcript_rows(&st, &buf), read);
        // Back down to the bottom (PgDn, as many as it takes): it follows again.
        for _ in 0..20 {
            if st.view.scroll == 0 {
                break;
            }
            st.handle_key(key(KeyCode::PageDown));
            draw(&mut st, 80, 30);
        }
        assert_eq!((st.view.scroll, st.view.anchor.clone()), (0, None));
        st.apply_event(&thread_item(&a.text("fresh")));
        let buf = draw(&mut st, 80, 30);
        let rows = transcript_rows(&st, &buf);
        assert!(rows.iter().any(|r| r.contains("fresh")), "{rows:?}");
        assert!(!screen_text(&buf).contains("below"));
    }

    /// The wheel scrolls up away from the bottom and back down to it, where the view follows
    /// again.
    #[test]
    fn the_wheel_back_to_the_bottom_follows_again() {
        let (mut st, mut a) = alpha_replies(40, |i| format!("line-{i:02}"));
        draw(&mut st, 80, 30);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let up = mouse(MouseEventKind::ScrollUp, tr.area.x + 1, tr.area.y + 1);
        let down = mouse(MouseEventKind::ScrollDown, tr.area.x + 1, tr.area.y + 1);
        let screen = Buffer::empty(ratatui::layout::Rect::default());
        for _ in 0..3 {
            st.handle_mouse(up, &screen);
        }
        let buf = draw(&mut st, 80, 30);
        let read = transcript_rows(&st, &buf);
        let below = st.view.scroll;
        st.apply_event(&thread_item(&a.text("while up")));
        let buf = draw(&mut st, 80, 30);
        assert_eq!(transcript_rows(&st, &buf), read);
        // Its two rows (a blank one, the reply) are below too.
        assert_eq!(st.view.scroll, below + 2);
        let mut notches = 0;
        while st.view.scroll > 0 {
            st.handle_mouse(down, &screen);
            draw(&mut st, 80, 30);
            notches += 1;
            assert!(notches <= 3, "{}", st.view.scroll);
        }
        // 9 rows: 3 notches up, less the working cursor's 2 rows, plus the reply's 2.
        assert_eq!(notches, 3);
        let buf = draw(&mut st, 80, 30);
        assert_eq!((st.view.scroll, st.view.anchor.clone()), (0, None));
        st.apply_event(&thread_item(&a.text("now following")));
        let buf2 = draw(&mut st, 80, 30);
        assert!(transcript_rows(&st, &buf2)
            .iter()
            .any(|r| r.contains("now following")));
        assert!(transcript_rows(&st, &buf)
            .iter()
            .any(|r| r.contains("while up")));
    }

    /// `End` jumps to the bottom and follows; `Home` goes to the top. While a line is typed
    /// they move its cursor, and `Ctrl-End` jumps instead; on an empty line `End` jumps.
    #[test]
    fn end_follows_and_home_goes_to_the_top() {
        let (mut st, mut a) = alpha_replies(40, |i| format!("line-{i:02}"));
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::Home));
        let buf = draw(&mut st, 80, 30);
        let max = st.view.panes.get(&PaneId::Transcript).unwrap().max_scroll;
        assert_eq!(st.view.scroll, max);
        let rows = transcript_rows(&st, &buf);
        assert!(rows.iter().any(|r| r.contains("line-00")), "{rows:?}");
        st.apply_event(&thread_item(&a.text("one more")));
        let buf = draw(&mut st, 80, 30);
        assert_eq!(transcript_rows(&st, &buf), rows, "the top stays");
        st.handle_key(key(KeyCode::End));
        assert_eq!((st.view.scroll, st.view.anchor.clone()), (0, None));
        let buf = draw(&mut st, 80, 30);
        assert!(transcript_rows(&st, &buf)
            .iter()
            .any(|r| r.contains("one more")));
        // End and Home move the cursor of a line that has text.
        for c in "hi".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        st.handle_key(key(KeyCode::PageUp));
        draw(&mut st, 80, 30);
        let up = (st.view.scroll, st.view.anchor.clone());
        assert!(up.0 > 0);
        st.handle_key(key(KeyCode::Home));
        assert_eq!(st.view.input.cursor(), 0);
        st.handle_key(key(KeyCode::End));
        assert_eq!(st.view.input.cursor(), 2);
        let buf = draw(&mut st, 80, 30);
        assert_eq!((st.view.scroll, st.view.anchor.clone()), up);
        assert!(screen_text(&buf).contains("below · Ctrl-End"));
        st.handle_key(mods(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!((st.view.scroll, st.view.anchor.clone()), (0, None));
        assert_eq!(st.view.input.as_str(), "hi");
        // On an empty line, End jumps.
        st.handle_key(key(KeyCode::PageUp));
        draw(&mut st, 80, 30);
        st.view.input.clear();
        st.handle_key(key(KeyCode::End));
        assert_eq!((st.view.scroll, st.view.anchor.clone()), (0, None));
    }

    /// Resized, a view scrolled up keeps the reply it was on at its top, though every reply
    /// wraps to another number of rows.
    #[test]
    fn a_view_scrolled_up_stays_on_its_reply_when_resized() {
        let words = "word ".repeat(24);
        let (mut st, _) = alpha_replies(30, |i| format!("para-{i:02} {words}"));
        draw(&mut st, 160, 33);
        st.handle_key(key(KeyCode::PageUp));
        st.handle_key(key(KeyCode::PageUp));
        let first = |st: &AppState, buf: &Buffer| {
            transcript_rows(st, buf)
                .iter()
                .find_map(|r| r.find("para-").map(|at| r[at..at + 7].to_string()))
                .unwrap()
        };
        let buf = draw(&mut st, 160, 33);
        let wide = transcript_rows(&st, &buf);
        let label = first(&st, &buf);
        let anchor = st.view.anchor.clone().unwrap();
        assert!(anchor.chars > 0, "inside a reply: {anchor:?}");
        for w in [60, 45, 34, 120] {
            let buf = draw(&mut st, w, 33);
            let top = st.view.panes.get(&PaneId::Transcript).unwrap().top.clone();
            let top = top.unwrap();
            // The top row is still one of the same reply's rows ...
            assert_eq!((top.entry, top.rows), (anchor.entry, 0), "at {w}");
            // ... so the first reply that starts on screen is the next one.
            assert_eq!(first(&st, &buf), label, "at {w} columns");
            assert!(st.view.scroll > 0, "at {w}");
        }
        // Back at its width, it is exactly where it was.
        let buf = draw(&mut st, 160, 33);
        assert_eq!(transcript_rows(&st, &buf), wide);
    }

    /// Entries dropped from the front of a long transcript ([`TRANSCRIPT_MAX_ENTRIES`]) do not
    /// move a view scrolled up: near its end, nor inside the entry that becomes the first one
    /// kept (which loses the blank row before it).
    #[test]
    fn a_view_scrolled_up_stays_put_when_old_entries_are_dropped() {
        let long = (0..60)
            .map(|k| format!("word{k}"))
            .collect::<Vec<_>>()
            .join(" ");
        let (mut st, mut a) = alpha_replies(TRANSCRIPT_MAX_ENTRIES, |i| match i {
            11 => long.clone(),
            _ => format!("e{i}"),
        });
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::PageUp));
        let read = drawn_rows(&mut st, 80, 30);
        for i in 0..10 {
            st.apply_event(&thread_item(&a.text(&format!("n{i}"))));
        }
        let t = st.transcript.as_ref().unwrap();
        assert_eq!((t.entries.len(), t.ids[0]), (TRANSCRIPT_MAX_ENTRIES, 10));
        assert_eq!(drawn_rows(&mut st, 80, 30), read);
        // Into the second entry's text, past its first row.
        st.handle_key(key(KeyCode::Home));
        draw(&mut st, 80, 30);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let wheel = mouse(MouseEventKind::ScrollDown, tr.area.x + 1, tr.area.y + 1);
        st.handle_mouse(wheel, &Buffer::empty(ratatui::layout::Rect::default()));
        let read = drawn_rows(&mut st, 80, 30);
        assert!(
            read[0].contains("word") && !read[0].contains("word0 "),
            "{read:?}"
        );
        st.apply_event(&thread_item(&a.text("one more")));
        assert_eq!(st.transcript.as_ref().unwrap().ids[0], 11);
        assert_eq!(drawn_rows(&mut st, 80, 30), read);
    }

    /// Diving in and back, each conversation opens at its bottom, following.
    #[test]
    fn a_dive_and_back_open_at_the_bottom() {
        let (mut st, _) = alpha_replies(40, |i| format!("line-{i:02}"));
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::PageUp));
        draw(&mut st, 80, 30);
        assert!(st.view.scroll > 0);
        st.back();
        assert_eq!((st.view.scroll, st.view.anchor.clone()), (0, None));
        draw(&mut st, 80, 30);
        st.dive("t-alpha");
        assert_eq!((st.view.scroll, st.view.anchor.clone()), (0, None));
        let buf = draw(&mut st, 80, 30);
        assert!(transcript_rows(&st, &buf)
            .iter()
            .any(|r| r.contains("line-39")));
    }

    /// On a phone-sized terminal the pane's last row still says what is below, shortened.
    #[test]
    fn the_indicator_fits_a_narrow_terminal() {
        assert_eq!(
            crate::ui::below_text(12, "End", 60),
            " ↓ 12 lines below · End to follow "
        );
        assert_eq!(
            crate::ui::below_text(1, "End", 60),
            " ↓ 1 line below · End to follow "
        );
        assert_eq!(crate::ui::below_text(12, "End", 26), " ↓ 12 below · End ");
        assert_eq!(crate::ui::below_text(1234, "Ctrl-End", 20), " ↓ 1234 more ");
        assert_eq!(crate::ui::below_text(12, "End", 6), " ↓ 12 ");
        assert_eq!(crate::ui::below_text(12, "End", 3), " ↓ ");
        assert_eq!(crate::ui::below_text(12, "End", 2), "");
        for w in [30, 34, 40, 45] {
            let (mut st, _) = alpha_replies(40, |i| format!("line-{i:02}"));
            draw(&mut st, w, 40);
            st.handle_key(key(KeyCode::PageUp));
            let buf = draw(&mut st, w, 40);
            let text = screen_text(&buf);
            let shown = if w >= 34 {
                "↓ 9 lines below · End to follow"
            } else {
                "↓ 9 below · End"
            };
            assert!(text.contains(shown), "{w}:\n{text}");
        }
    }

    /// After `Home`, PgDn and the wheel move down from the top: the view is anchored on the
    /// row drawn there, not on more rows above the first entry than there are.
    #[test]
    fn home_then_page_down_or_the_wheel_moves_down() {
        let (mut st, _) = alpha_replies(40, |i| format!("line-{i:02}"));
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::Home));
        let top = drawn_rows(&mut st, 80, 30);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        assert!(tr.max_scroll > SCROLL_STEP, "{}", tr.max_scroll);
        assert_eq!(st.view.scroll, tr.max_scroll);
        st.handle_key(key(KeyCode::PageDown));
        let down = drawn_rows(&mut st, 80, 30);
        assert_eq!(st.view.scroll, tr.max_scroll - SCROLL_STEP);
        assert_eq!(down[..top.len() - SCROLL_STEP], top[SCROLL_STEP..]);
        st.handle_key(key(KeyCode::Home));
        assert_eq!(drawn_rows(&mut st, 80, 30), top);
        let wheel = mouse(MouseEventKind::ScrollDown, tr.area.x + 1, tr.area.y + 1);
        st.handle_mouse(wheel, &Buffer::empty(ratatui::layout::Rect::default()));
        let down = drawn_rows(&mut st, 80, 30);
        assert_eq!(st.view.scroll, tr.max_scroll - WHEEL_STEP);
        assert_eq!(down[..top.len() - WHEEL_STEP], top[WHEEL_STEP..]);
    }

    /// Paragraphs `from..to` of words no other paragraph has (`w3q0 w3q1 ...`), a blank line
    /// between them.
    fn paragraphs(from: usize, to: usize) -> String {
        (from..to)
            .map(|p| {
                (0..20)
                    .map(|k| format!("w{p}q{k}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Inside a reply still streaming in, a view resized and given more of the reply stays on
    /// the same text: its top row holds the word the first width's top row began with, and
    /// back at that width it is exactly where it was.
    #[test]
    fn a_view_inside_a_streaming_reply_stays_on_its_text_through_resizes() {
        let (mut st, mut a) = alpha_replies(5, |i| format!("before-{i}"));
        st.apply_event(&delta(&mut a, &paragraphs(0, 30)));
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::PageUp));
        st.handle_key(key(KeyCode::PageUp));
        let wide = drawn_rows(&mut st, 80, 30);
        // The top row, or the paragraph under a blank one.
        let at = usize::from(wide[0].is_empty());
        let word = wide[at].split_whitespace().next().unwrap().to_string();
        assert!(word.starts_with('w'), "{wide:?}");
        for (w, more) in [(50, 30), (37, 33), (61, 36)] {
            let rows = drawn_rows(&mut st, w, 30);
            assert_eq!(rows[0].is_empty(), at == 1, "at {w}: {rows:?}");
            assert!(
                rows[at].split_whitespace().any(|r| r == word),
                "at {w}, {word}: {rows:?}"
            );
            st.apply_event(&delta(
                &mut a,
                &format!("\n\n{}", paragraphs(more, more + 3)),
            ));
            assert_eq!(drawn_rows(&mut st, w, 30), rows, "at {w}");
        }
        assert_eq!(drawn_rows(&mut st, 80, 30), wide);
    }

    /// A run of tool calls `calls` (`c0`, `c1`, ...), each `Bash` with `input`, as `a`'s events.
    fn tool_calls(
        a: &mut ThreadScript,
        calls: std::ops::Range<usize>,
        input: impl Fn(usize) -> String,
    ) -> Vec<StreamItem> {
        calls
            .map(|c| {
                thread_item(&a.event(EventBody::AgentToolCall(AgentToolCall {
                    call_id: format!("c{c}"),
                    tool: "Bash".into(),
                    input_summary: input(c),
                    ..Default::default()
                })))
            })
            .collect()
    }

    /// Tool calls expanded (Ctrl-O), a view on one of a run of calls stays on it when a call
    /// before it fails (its `⎿ failed` row goes in above the view).
    #[test]
    fn a_view_on_an_expanded_call_stays_put_when_an_earlier_call_fails() {
        let (mut st, mut a) = alpha_replies(1, |i| format!("before-{i}"));
        for ev in tool_calls(&mut a, 0..40, |c| format!("step-{c:02}")) {
            st.apply_event(&ev);
        }
        for i in 0..10 {
            st.apply_event(&thread_item(&a.text(&format!("after-{i}"))));
        }
        st.handle_key(ctrl('o'));
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::PageUp));
        st.handle_key(key(KeyCode::PageUp));
        let read = drawn_rows(&mut st, 80, 30);
        let step = read[0]
            .split_once("Bash(step-")
            .and_then(|(_, n)| n[..2].parse::<usize>().ok());
        assert!(step.is_some_and(|n| n > 5), "{read:?}");
        st.apply_event(&thread_item(&a.event(EventBody::AgentToolResult(
            AgentToolResult {
                call_id: "c0".into(),
                ok: false,
                ..Default::default()
            },
        ))));
        let buf = draw(&mut st, 80, 30);
        assert_eq!(transcript_rows(&st, &buf), read);
        assert!(screen_text(&buf).contains("lines below"));
    }

    /// Folded with Ctrl-O while the view is inside a long expanded tool input, the view goes
    /// to the call's folded line (not as many rows past it, into the replies below) and stays
    /// there; expanded again, it is back where it was.
    #[test]
    fn folding_from_inside_an_expanded_call_goes_to_its_folded_line() {
        let (mut st, mut a) = alpha_replies(1, |i| format!("before-{i}"));
        let input = |_| {
            (0..400)
                .map(|k| format!("arg{k}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        for ev in tool_calls(&mut a, 0..1, input) {
            st.apply_event(&ev);
        }
        for i in 0..40 {
            st.apply_event(&thread_item(&a.text(&format!("after-{i}"))));
        }
        st.handle_key(ctrl('o'));
        draw(&mut st, 80, 30);
        // Up into the call's input, past its first row.
        let mut read = Vec::new();
        for _ in 0..20 {
            st.handle_key(key(KeyCode::PageUp));
            read = drawn_rows(&mut st, 80, 30);
            if read[0].contains("arg") {
                break;
            }
        }
        assert!(
            read[0].contains("arg") && !read[0].contains("Bash"),
            "{read:?}"
        );
        st.handle_key(ctrl('o'));
        let folded = drawn_rows(&mut st, 80, 30);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap();
        assert_eq!(tr.links[0].as_deref(), Some("tools:c0"), "{folded:?}");
        assert!(folded[2].contains("after-0"), "{folded:?}");
        assert_eq!(drawn_rows(&mut st, 80, 30), folded);
        st.handle_key(ctrl('o'));
        // (The status bar says what Ctrl-O did, on rows the pane gives up.)
        let rows = drawn_rows(&mut st, 80, 30);
        assert_eq!(rows[0], read[0]);
    }

    /// A reply finished with other text than streamed in: a view on text the final reply
    /// still has stays on it; one on text it no longer has goes to the reply's last row, not
    /// into the replies after it, and stays scrolled up.
    #[test]
    fn a_final_reply_that_differs_keeps_the_view_in_it() {
        let streamed = |ups: usize| {
            let (mut st, mut a) = alpha_replies(3, |i| format!("before-{i}"));
            st.apply_event(&delta(&mut a, &paragraphs(0, 30)));
            draw(&mut st, 80, 30);
            for _ in 0..ups {
                st.handle_key(key(KeyCode::PageUp));
            }
            let read = drawn_rows(&mut st, 80, 30);
            (st, a, read)
        };
        // The view is on the first fifteen paragraphs, which the final reply keeps.
        let (mut st, mut a, read) = streamed(5);
        assert!(read.iter().all(|r| !r.contains("w15q")), "{read:?}");
        st.apply_event(&thread_item(
            &a.text(&format!("{}\n\nended otherwise", paragraphs(0, 15))),
        ));
        assert_eq!(drawn_rows(&mut st, 80, 30), read);
        // The view is further down than the final reply goes.
        let (mut st, mut a, read) = streamed(2);
        assert!(read.iter().any(|r| r.contains("w20q")), "{read:?}");
        st.apply_event(&thread_item(&a.text(&paragraphs(0, 3))));
        for i in 0..30 {
            st.apply_event(&thread_item(&a.text(&format!("after-{i}"))));
        }
        let rows = drawn_rows(&mut st, 80, 30);
        assert!(rows[0].contains("w2q19"), "{rows:?}");
        assert!(rows[2].contains("after-0"), "{rows:?}");
        assert!(st.view.scroll > 0);
    }

    /// A hub update of request `id` in `a`'s transcript, waiting.
    fn request_update(a: &mut ThreadScript, id: &str) -> StreamItem {
        thread_item(
            &a.event(EventBody::ConversationUpdate(
                twr_core::event::ConversationUpdate {
                    text: format!("alpha needs permission ({id})"),
                    about_thread: Some("t-alpha".into()),
                    ext: serde_json::json!({ "twr": { "attn_id": id } })
                        .as_object()
                        .cloned(),
                    ..Default::default()
                },
            )),
        )
    }

    /// A request raised again (a new revision) takes its older update out of the transcript:
    /// a view on a reply after it stays on that reply.
    #[test]
    fn a_view_stays_put_when_a_request_raised_again_drops_its_older_update() {
        let (mut st, mut a) = alpha_replies(3, |i| format!("before-{i}"));
        st.apply_event(&request_update(&mut a, "x1"));
        for i in 0..40 {
            st.apply_event(&thread_item(&a.text(&format!("line-{i:02}"))));
        }
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::PageUp));
        let read = drawn_rows(&mut st, 80, 30);
        let entries = st.transcript.as_ref().unwrap().entries.len();
        st.apply_event(&request_update(&mut a, "x1"));
        assert_eq!(st.transcript.as_ref().unwrap().entries.len(), entries);
        assert_eq!(drawn_rows(&mut st, 80, 30), read);
    }

    /// A transcript at its most entries, scrolled up while a replay is applied that drops
    /// entries from its front: a frame drawn meanwhile shows the entries as the replay began,
    /// at the same place; scrolling there moves the view by the rows scrolled, and the
    /// replay's end moves nothing.
    #[test]
    fn a_view_scrolled_up_during_a_replay_stays_put() {
        let (mut st, mut a) = alpha_replies(TRANSCRIPT_MAX_ENTRIES, |i| format!("e{i}"));
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::PageUp));
        let read = drawn_rows(&mut st, 80, 30);
        let replay: Vec<Event> = (0..20).map(|i| a.text(&format!("replayed-{i}"))).collect();
        let t = st.transcript.as_mut().unwrap();
        t.resume(t.last_seq, replay[19].seq);
        for ev in &replay[..10] {
            st.apply_event(&thread_item(ev));
        }
        let t = st.transcript.as_ref().unwrap();
        assert!(t.catching_up());
        assert_eq!(t.ids[0], 10, "the replay dropped entries from the front");
        assert_eq!(drawn_rows(&mut st, 80, 30), read);
        st.handle_key(key(KeyCode::PageUp));
        let up = drawn_rows(&mut st, 80, 30);
        assert_eq!(up[SCROLL_STEP..], read[..read.len() - SCROLL_STEP]);
        for ev in &replay[10..] {
            st.apply_event(&thread_item(ev));
        }
        assert!(!st.transcript.as_ref().unwrap().catching_up());
        assert_eq!(drawn_rows(&mut st, 80, 30), up);
    }

    /// More than 65,535 rows below a view scrolled up (`u16::MAX`): the pane's last row counts
    /// them all, and PgUp and PgDn still move the view.
    #[test]
    fn a_view_more_than_u16_rows_up_still_scrolls_both_ways() {
        let (mut st, mut a) = alpha_replies(40, |i| format!("line-{i:02}"));
        draw(&mut st, 80, 30);
        st.handle_key(key(KeyCode::PageUp));
        let read = drawn_rows(&mut st, 80, 30);
        let long = (0..70_000)
            .map(|k| format!("r{k}"))
            .collect::<Vec<_>>()
            .join("\n");
        st.apply_event(&thread_item(&a.text(&long)));
        let buf = draw(&mut st, 80, 30);
        assert_eq!(transcript_rows(&st, &buf), read);
        assert!(st.view.scroll > 70_000, "{}", st.view.scroll);
        let shown = format!("↓ {} lines below", st.view.scroll + 1);
        assert!(screen_text(&buf).contains(&shown), "{shown}");
        st.handle_key(key(KeyCode::PageUp));
        let up = drawn_rows(&mut st, 80, 30);
        assert_eq!(up[SCROLL_STEP..], read[..read.len() - SCROLL_STEP]);
        st.handle_key(key(KeyCode::PageDown));
        st.handle_key(key(KeyCode::PageDown));
        let down = drawn_rows(&mut st, 80, 30);
        assert_eq!(down[..read.len() - SCROLL_STEP], read[SCROLL_STEP..]);
    }

    /// `n` words `{word}{k}`, one line.
    fn words(word: &str, n: usize) -> String {
        (0..n)
            .map(|k| format!("{word}{k}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The first word of `row` that starts with `word`.
    fn first_word<'a>(row: &'a str, word: &str) -> Option<&'a str> {
        row.split_whitespace().find(|w| w.starts_with(word))
    }

    /// Wheels the main pane down from its top, a notch at a time, until its top row shows a
    /// word starting with `word` other than `{word}0` (inside a wrapped text, past its first
    /// row); its rows then, drawn `w` x `h`.
    fn wheel_into(st: &mut AppState, word: &str, w: u16, h: u16) -> Vec<String> {
        draw(st, w, h);
        st.handle_key(key(KeyCode::Home));
        draw(st, w, h);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let wheel = mouse(MouseEventKind::ScrollDown, tr.area.x + 1, tr.area.y + 1);
        for _ in 0..40 {
            st.handle_mouse(wheel, &Buffer::empty(ratatui::layout::Rect::default()));
            let rows = drawn_rows(st, w, h);
            if first_word(&rows[0], word).is_some_and(|f| f != format!("{word}0")) {
                return rows;
            }
        }
        panic!("no row of {word}");
    }

    /// Inside a wrapped summary or option of a request drawn in the conversation (in the entry
    /// of the update that raised it, not selected), a view resized keeps the same text at its
    /// top, and back at its width is exactly where it was: also when the short options before
    /// a long one share a row at one width and not at another.
    #[test]
    fn a_view_inside_a_request_stays_on_its_text_when_resized() {
        let mut st = api_and_web_waiting();
        let mut a = ThreadScript::new("t-api");
        a.at(40);
        let mut pa = a.raise("pa", Permission);
        if let EventBody::AttentionRaised(p) = &mut pa.body {
            p.summary = words("sumq", 90);
            p.options = vec![
                "allow this command once".into(),
                "deny this command".into(),
                words("optq", 60),
            ];
        }
        control(&mut st, &[pa]);
        let mut o = ThreadScript::new("t-orch");
        o.at(50);
        st.apply_event(&request_update(&mut o, "pa"));
        for i in 0..30 {
            st.apply_event(&thread_item(&o.text(&format!("reply {i}"))));
        }
        // `1 allow this command once  2 deny this command`: 46 cells, in a box 4 wider.
        let mut shared = Vec::new();
        for word in ["sumq", "optq"] {
            let wide = wheel_into(&mut st, word, 120, 30);
            assert!(st.view.request.is_none());
            let at = first_word(&wide[0], word).unwrap().to_string();
            for w in [90, 70, 48] {
                let rows = drawn_rows(&mut st, w, 30);
                assert!(
                    rows[0].split_whitespace().any(|r| r == at),
                    "{word} at {w}, {at}: {rows:?}"
                );
                let tr = st.view.panes.get(&PaneId::Transcript).unwrap();
                shared.push(tr.area.width >= 50);
            }
            assert_eq!(drawn_rows(&mut st, 120, 30), wide, "{word}");
        }
        assert!(
            shared.contains(&true) && shared.contains(&false),
            "{shared:?}"
        );
    }

    /// A request read in the conversation is answered: the view stays on its update; raised
    /// again under the same id (its new card at the bottom), it does not pull the view there.
    #[test]
    fn a_request_answered_then_raised_again_does_not_pull_the_view() {
        let (mut st, mut a, mut o) = reading_pa();
        answer_pa(&mut st, &mut a, &mut o);
        let answered = drawn_rows(&mut st, 120, 30);
        assert!(!answered.iter().any(|r| r.contains("sumq")), "{answered:?}");
        assert!(st.view.scroll > 0);
        assert_eq!(st.view.anchor.as_ref().unwrap().request, None);
        let pa = long_pa(&mut a);
        control(&mut st, &[pa]);
        st.apply_event(&request_update(&mut o, "pa"));
        assert_eq!(drawn_rows(&mut st, 120, 30), answered);
    }

    /// `s` with no whitespace nor a box's frame (`│`): on the screen a wide character's second
    /// cell is a space.
    fn solid(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && *c != '│')
            .collect()
    }

    /// Wheels the main pane down from its top until its top row is text of `text` past its
    /// middle (past many rows of it, whose counts add up); where in `text` that row starts
    /// (both [`solid`]) and the rows drawn `w` x `h`.
    fn wheel_to_middle(st: &mut AppState, text: &str, w: u16, h: u16) -> (usize, Vec<String>) {
        let text = solid(text);
        draw(st, w, h);
        st.handle_key(key(KeyCode::Home));
        draw(st, w, h);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let wheel = mouse(MouseEventKind::ScrollDown, tr.area.x + 1, tr.area.y + 1);
        for _ in 0..200 {
            st.handle_mouse(wheel, &Buffer::empty(ratatui::layout::Rect::default()));
            let rows = drawn_rows(st, w, h);
            let row = solid(&rows[0]);
            match text.find(&row) {
                Some(at) if !row.is_empty() && at >= text.len() / 2 => return (at, rows),
                _ => {}
            }
        }
        panic!("not in the middle of {text}");
    }

    /// Asserts that the top row drawn `w` x `h` holds the character at `at` of `text` (both
    /// [`solid`]).
    fn assert_top_holds(st: &mut AppState, text: &str, at: usize, w: u16, h: u16) {
        let text = solid(text);
        let rows = drawn_rows(st, w, h);
        let row = solid(&rows[0]);
        let start = text
            .find(&row)
            .unwrap_or_else(|| panic!("at {w}: {:?}", rows[0]));
        assert!(
            (start..start + row.len()).contains(&at),
            "at {w}: {:?} for {}",
            rows[0],
            text[at..].chars().take(12).collect::<String>()
        );
    }

    /// `n` tokens of a word, joined emoji, an emoji with a skin tone and a letter with a
    /// combining mark (`word007👩‍💻👍🏽é`), each after `sep`.
    fn emoji_tokens(word: &str, n: usize, sep: &str) -> String {
        (0..n)
            .map(|k| format!("{word}{k:03}👩‍💻👍🏽e\u{301}{sep}"))
            .collect()
    }

    /// In a long code line of joined emoji, emoji with a skin tone and letters with a
    /// combining mark, a view resized keeps the same text at its top: each of them counts
    /// once, however many characters and cells it takes.
    #[test]
    fn a_view_inside_a_code_line_of_emoji_stays_on_its_text_when_resized() {
        let line = emoji_tokens("word", 200, " ");
        let (mut st, mut a) = alpha_replies(5, |i| format!("before-{i}"));
        st.apply_event(&thread_item(&a.text(&format!("```\n{line}\n```"))));
        for i in 0..20 {
            st.apply_event(&thread_item(&a.text(&format!("after-{i}"))));
        }
        let (at, wide) = wheel_to_middle(&mut st, &line, 120, 30);
        for w in [90, 70, 50] {
            assert_top_holds(&mut st, &line, at, w, 30);
        }
        assert_eq!(drawn_rows(&mut st, 120, 30), wide);
    }

    /// In a request's summary and option of such tokens with no space between them, mostly
    /// emoji (wrapped inside the "word", never inside a joined emoji), a view resized keeps
    /// the same text at its top.
    #[test]
    fn a_view_inside_an_unbroken_request_of_emoji_stays_on_its_text_when_resized() {
        let dense = |word: &str| emoji_tokens(word, 300, "👩‍💻👍🏽👩‍💻👍🏽");
        let (summary, option) = (dense("sumq"), dense("optq"));
        let mut st = api_and_web_waiting();
        let mut a = ThreadScript::new("t-api");
        a.at(40);
        let mut pa = a.raise("pa", Permission);
        if let EventBody::AttentionRaised(p) = &mut pa.body {
            p.summary = summary.clone();
            p.options = vec!["allow".into(), option.clone()];
        }
        control(&mut st, &[pa]);
        let mut o = ThreadScript::new("t-orch");
        o.at(50);
        st.apply_event(&request_update(&mut o, "pa"));
        for i in 0..30 {
            st.apply_event(&thread_item(&o.text(&format!("reply {i}"))));
        }
        for text in [&summary, &option] {
            let (at, wide) = wheel_to_middle(&mut st, text, 120, 30);
            for w in [90, 70, 50, 40] {
                assert_top_holds(&mut st, text, at, w, 30);
            }
            assert_eq!(drawn_rows(&mut st, 120, 30), wide);
        }
    }

    /// `pa`, the api's request with a long summary, raised (`api_and_web_waiting` raised it
    /// once before), its update in the orchestrator's transcript `o` with 30 replies after it,
    /// and the view wheeled into its summary.
    fn reading_pa() -> (AppState, ThreadScript, ThreadScript) {
        let mut st = api_and_web_waiting();
        let mut a = ThreadScript::new("t-api");
        a.at(40);
        let pa = long_pa(&mut a);
        control(&mut st, &[pa]);
        let mut o = ThreadScript::new("t-orch");
        o.at(50);
        st.apply_event(&request_update(&mut o, "pa"));
        for i in 0..30 {
            st.apply_event(&thread_item(&o.text(&format!("reply {i}"))));
        }
        wheel_into(&mut st, "sumq", 120, 30);
        (st, a, o)
    }

    /// A raise of `pa` with a long summary.
    fn long_pa(a: &mut ThreadScript) -> Event {
        let mut pa = a.raise("pa", Permission);
        if let EventBody::AttentionRaised(p) = &mut pa.body {
            p.summary = words("sumq", 90);
        }
        pa
    }

    /// `pa` answered: its end in the control stream and in the transcript (onto its update).
    fn answer_pa(st: &mut AppState, a: &mut ThreadScript, o: &mut ThreadScript) {
        control(st, &[a.resolve("pa")]);
        st.apply_event(&thread_item(
            &o.event(EventBody::ConversationUpdate(
                twr_core::event::ConversationUpdate {
                    text: "api: allowed".into(),
                    ext: serde_json::json!({ "twr": { "attn_id": "pa", "resolved": true } })
                        .as_object()
                        .cloned(),
                    ..Default::default()
                },
            )),
        ));
    }

    /// A request being read is raised again before it is answered: its update goes, a new one
    /// comes at the bottom with its card, and the view follows the card there. Answered then,
    /// the view goes to that update, not to where the old one was.
    #[test]
    fn a_request_read_through_a_new_revision_then_answered_stays_at_its_update() {
        let (mut st, mut a, mut o) = reading_pa();
        // Its rows on screen, to its bottom border (what is below it changes).
        let read = drawn_rows(&mut st, 120, 30);
        let card = read.iter().position(|r| r.starts_with('╰')).unwrap() + 1;
        let pa = long_pa(&mut a);
        control(&mut st, &[pa]);
        st.apply_event(&request_update(&mut o, "pa"));
        for i in 0..30 {
            st.apply_event(&thread_item(&o.text(&format!("later {i}"))));
        }
        let moved = drawn_rows(&mut st, 120, 30);
        assert_eq!(
            moved[..card],
            read[..card],
            "the card moved, the view with it"
        );
        assert!(moved.iter().any(|r| r.contains("later 0")), "{moved:?}");
        answer_pa(&mut st, &mut a, &mut o);
        let rows = drawn_rows(&mut st, 120, 30);
        assert!(rows[0].contains("(pa)"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("later 0")), "{rows:?}");
        assert!(st.view.scroll > 0);
    }

    /// A request being read is answered and raised again under the same id while the grid is
    /// open (no frame draws the conversation meanwhile): back in the conversation, the view is
    /// at the update of the request read, not on the new card.
    #[test]
    fn a_request_answered_and_raised_again_behind_the_grid_does_not_pull_the_view() {
        let (mut st, mut a, mut o) = reading_pa();
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        draw(&mut st, 120, 30);
        answer_pa(&mut st, &mut a, &mut o);
        draw(&mut st, 120, 30);
        let pa = long_pa(&mut a);
        control(&mut st, &[pa]);
        st.apply_event(&request_update(&mut o, "pa"));
        draw(&mut st, 120, 30);
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Stack);
        let rows = drawn_rows(&mut st, 120, 30);
        assert!(rows[0].contains("(pa)"), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("sumq")), "{rows:?}");
        assert!(st.view.scroll > 0);
    }

    /// Requests drawn above the conversation's entries (no update of theirs is in it; here it
    /// has none at all): a view inside one stays on it while a request below it is answered,
    /// another one's full text arrives, and the terminal is resized.
    #[test]
    fn a_view_inside_a_request_above_the_entries_stays_on_it() {
        let store = twr_core::blob::MemoryBlobStore::new();
        let mut st = api_and_web_waiting();
        let mut a = ThreadScript::new("t-api");
        let mut w = ThreadScript::new("t-web");
        a.at(40);
        w.at(40);
        let mut pa = a.raise("pa", Permission);
        if let EventBody::AttentionRaised(p) = &mut pa.body {
            p.summary = words("aaq", 150);
        }
        let mut pw = w.raise("pw", Permission);
        if let EventBody::AttentionRaised(p) = &mut pw.body {
            p.summary = words("wwq", 150);
        }
        let mut big = a.raise("pc", Permission);
        if let EventBody::AttentionRaised(p) = &mut big.body {
            p.summary = words("ccq", 800);
        }
        control(&mut st, &[pa, pw, capped(&big, &store)]);
        let pc = st.attention.get("pc").unwrap().clone();
        assert!(st.incomplete(&pc).is_some());
        assert!(st.transcript.as_ref().is_none_or(|t| t.entries.is_empty()));
        // Which request is drawn first.
        let all = drawn_rows(&mut st, 120, 200).join(" ");
        let at = |word: &str| all.find(word).unwrap();
        let (first, other, word) = if at("aaq0") < at("wwq0") {
            ("pa", "pw", "aaq")
        } else {
            ("pw", "pa", "wwq")
        };
        assert!(at(word) < at("ccq0"), "{all}");
        let read = wheel_into(&mut st, word, 120, 30);
        assert!(st.view.panes.get(&PaneId::Transcript).unwrap().max_scroll > 10);
        // Its rows on screen, to its bottom border (what is below it changes).
        let card = read.iter().position(|r| r.starts_with('╰')).unwrap() + 1;
        let read = read[..card].to_vec();
        let shown = |st: &mut AppState, w: u16| drawn_rows(st, w, 30)[..card].to_vec();
        // The other request is answered.
        let resolve = if other == "pa" {
            a.resolve(other)
        } else {
            w.resolve(other)
        };
        control(&mut st, &[resolve]);
        assert!(st.attention.get(other).is_none());
        assert_eq!(shown(&mut st, 120), read, "{other} answered");
        // The capped one's full text arrives.
        let mut full = raise_item(&st, &big);
        full.gseq = Some(pc.revision);
        st.hydrated("pc", pc.revision, Ok(full), Instant::now());
        assert!(st.incomplete(st.attention.get("pc").unwrap()).is_none());
        assert_eq!(shown(&mut st, 120), read, "pc in full");
        // Resized and back.
        let top = first_word(&read[0], word).unwrap().to_string();
        let rows = drawn_rows(&mut st, 80, 30);
        assert!(
            rows[0].split_whitespace().any(|r| r == top),
            "{first} at 80, {top}: {rows:?}"
        );
        assert_eq!(shown(&mut st, 120), read, "back at 120");
    }

    /// Expanded (Ctrl-O), wheeled into the middle of `text` (a later member of a group), the
    /// view folded with Ctrl-O goes to the group's line, `key`'s; expanded again, it is back
    /// on the same text.
    fn fold_and_unfold_keep(st: &mut AppState, text: &str, key: &str) {
        let (_, read) = wheel_to_middle(st, text, 120, 30);
        st.handle_key(ctrl('o'));
        let folded = drawn_rows(st, 120, 30);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap();
        assert_eq!(tr.links[0].as_deref(), Some(key), "{folded:?}");
        assert!(st.view.scroll > 0);
        st.handle_key(ctrl('o'));
        // (The status bar says what Ctrl-O did, on rows the pane gives up.)
        let rows = drawn_rows(st, 120, 30);
        assert_eq!(rows[0], read[0]);
    }

    /// Folded and expanded again from inside the second of two tool calls, the view is back
    /// in that call, at the same text.
    #[test]
    fn folding_and_unfolding_from_a_later_call_comes_back_to_it() {
        let (mut st, mut a) = alpha_replies(1, |i| format!("before-{i}"));
        let input = |c: usize| words(&format!("c{c}q"), 120);
        for ev in tool_calls(&mut a, 0..2, input) {
            st.apply_event(&ev);
        }
        for i in 0..40 {
            st.apply_event(&thread_item(&a.text(&format!("after-{i}"))));
        }
        st.handle_key(ctrl('o'));
        fold_and_unfold_keep(&mut st, &input(1), "tools:c0");
    }

    /// Folded and expanded again from inside the second of a run of answered requests, the
    /// view is back in that one, at the same text.
    #[test]
    fn folding_and_unfolding_from_a_later_answered_request_comes_back_to_it() {
        let (mut st, mut a) = alpha_replies(1, |i| format!("before-{i}"));
        let update =
            |a: &mut ThreadScript, text: &str, id: &str, resolved: bool| {
                thread_item(&a.event(EventBody::ConversationUpdate(
                twr_core::event::ConversationUpdate {
                    text: text.into(),
                    ext: serde_json::json!({ "twr": { "attn_id": id, "resolved": resolved } })
                        .as_object()
                        .cloned(),
                    ..Default::default()
                },
            )))
            };
        let text = |k: usize| words(&format!("r{k}q"), 120);
        for k in 0..3 {
            let id = format!("x{k}");
            st.apply_event(&update(&mut a, &text(k), &id, false));
            st.apply_event(&update(&mut a, "allowed", &id, true));
        }
        for i in 0..40 {
            st.apply_event(&thread_item(&a.text(&format!("after-{i}"))));
        }
        let seq = match &st.transcript.as_ref().unwrap().entries[1] {
            Entry::HubUpdate {
                seq,
                outcome: Some(_),
                ..
            } => *seq,
            e => panic!("{e:?}"),
        };
        st.handle_key(ctrl('o'));
        fold_and_unfold_keep(&mut st, &text(1), &format!("answers:{seq}"));
    }

    /// The orchestrator conversation with the sidebar hidden, its reply `text` and 40 short
    /// replies after it; its script, for more.
    fn orch_reply_and_more(text: &str) -> (AppState, ThreadScript) {
        let mut st = without_worker_requests(with_orch());
        st.view.sidebar_hidden = true;
        st.transcript.as_mut().unwrap().settle();
        let mut o = ThreadScript::new("t-orch");
        o.at(100);
        st.apply_event(&thread_item(&o.text(text)));
        for i in 0..40 {
            st.apply_event(&thread_item(&o.text(&format!("after-{i}"))));
        }
        (st, o)
    }

    /// Wheels the main pane down from its top until its top row holds one of `words`; its
    /// rows then, drawn `w` x `h`.
    fn wheel_to_any(st: &mut AppState, words: &[String], w: u16, h: u16) -> Vec<String> {
        draw(st, w, h);
        st.handle_key(key(KeyCode::Home));
        draw(st, w, h);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let wheel = mouse(MouseEventKind::ScrollDown, tr.area.x + 1, tr.area.y + 1);
        for _ in 0..100 {
            st.handle_mouse(wheel, &Buffer::empty(ratatui::layout::Rect::default()));
            let rows = drawn_rows(st, w, h);
            if rows[0]
                .split_whitespace()
                .any(|t| words.iter().any(|w| w == t))
            {
                return rows;
            }
        }
        panic!("no row of {words:?}");
    }

    /// A thread box that fits in a long block's 12 rows at one width collapses at a narrower
    /// one: a view on its later rows goes to its footer (not into the replies after it), and
    /// back at the first width it is where it was.
    #[test]
    fn a_view_in_a_block_that_collapses_when_narrowed_goes_to_its_footer_and_back() {
        let lines = (0..10)
            .map(|k| words(&format!("b{k}q"), 12))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut st, _) =
            orch_reply_and_more(&format!("<thread name=\"alpha\">\n{lines}\n</thread>"));
        let later: Vec<String> = (6..10).map(|k| format!("b{k}q0")).collect();
        let read = wheel_to_any(&mut st, &later, 120, 30);
        assert!(!read.iter().any(|r| r.contains("more lines")), "{read:?}");
        let narrow = drawn_rows(&mut st, 50, 30);
        assert!(narrow[0].contains("more lines"), "{narrow:?}");
        assert!(st.view.scroll > 0);
        assert_eq!(drawn_rows(&mut st, 120, 30), read);
    }

    /// In a reply of two long thread boxes, both expanded, a view inside the second stays on
    /// its text when the first is collapsed with `e`: each box counts its places by itself.
    #[test]
    fn collapsing_a_block_keeps_a_view_in_the_next_block_of_the_same_reply() {
        let boxed = |b: &str| {
            let lines = (0..30)
                .map(|k| words(&format!("{b}{k}q"), 3))
                .collect::<Vec<_>>()
                .join("\n");
            format!("<thread name=\"alpha\">\n{lines}\n</thread>")
        };
        let (mut st, _) = orch_reply_and_more(&format!("{}\n{}", boxed("x"), boxed("y")));
        draw(&mut st, 120, 30);
        let keys: Vec<String> = crate::ui::block_rows(&st)
            .iter()
            .filter_map(|r| r.2.as_deref().and_then(crate::ui::linked_block))
            .map(str::to_string)
            .fold(Vec::new(), |mut keys, k| {
                if keys.last() != Some(&k) {
                    keys.push(k);
                }
                keys
            });
        assert_eq!(keys.len(), 2, "{keys:?}");
        st.view.long_expanded.extend(keys.iter().cloned());
        let later: Vec<String> = (15..25).map(|k| format!("y{k}q0")).collect();
        let read = wheel_to_any(&mut st, &later, 120, 30);
        st.view.block = Some(keys[0].clone());
        st.handle_key(key(KeyCode::Enter));
        assert!(!st.view.long_expanded.contains(&keys[0]));
        assert_eq!(drawn_rows(&mut st, 120, 30), read);
    }

    /// A thread box of `lines` lines of `n` words each (`{b}{line}q{word}`).
    fn boxed(b: &str, lines: usize, n: usize) -> String {
        let lines = (0..lines)
            .map(|k| words(&format!("{b}{k}q"), n))
            .collect::<Vec<_>>()
            .join("\n");
        format!("<thread name=\"alpha\">\n{lines}\n</thread>")
    }

    /// The long blocks of the main pane, in order (their keys).
    fn block_keys(st: &AppState) -> Vec<String> {
        crate::ui::BlockView::new(st)
            .blocks()
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// Asserts that the main pane drawn `w` x 30 shows thread box `b` ([`boxed`]) from its
    /// head: its top border, then its first line.
    fn shows_head(st: &mut AppState, b: &str, w: u16) {
        let rows = drawn_rows(st, w, 30);
        assert!(
            rows[0].starts_with('╭') && rows[1].contains(&format!("{b}0q0")),
            "{b}: {rows:?}"
        );
    }

    /// `{` and `}` put the long block they select at the top of the main pane, its top
    /// border and first rows on screen, though the view was scrolled inside another block;
    /// `e` expands or collapses it there.
    #[test]
    fn block_keys_show_the_selected_block_from_its_head() {
        let (mut st, mut o) = orch_reply_and_more(&boxed("a", 30, 3));
        st.apply_event(&thread_item(&o.text(&boxed("b", 30, 3))));
        for i in 0..20 {
            st.apply_event(&thread_item(&o.text(&format!("tail-{i}"))));
        }
        draw(&mut st, 120, 30);
        let keys = block_keys(&st);
        assert_eq!(keys.len(), 2, "{keys:?}");
        st.view.long_expanded.extend(keys.iter().cloned());
        let inside: Vec<String> = (15..25).map(|k| format!("b{k}q0")).collect();
        wheel_to_any(&mut st, &inside, 120, 30);
        // The first press selects the last block shown.
        step(&mut st, KeyCode::Up);
        assert_eq!(st.view.block.as_ref(), Some(&keys[1]));
        shows_head(&mut st, "b", 120);
        step(&mut st, KeyCode::Up);
        assert_eq!(st.view.block.as_ref(), Some(&keys[0]));
        shows_head(&mut st, "a", 120);
        st.handle_key(key(KeyCode::Enter));
        assert!(!st.view.long_expanded.contains(&keys[0]));
        shows_head(&mut st, "a", 120);
        step(&mut st, KeyCode::Down);
        shows_head(&mut st, "b", 120);
        st.handle_key(key(KeyCode::Enter));
        assert!(!st.view.long_expanded.contains(&keys[1]));
        shows_head(&mut st, "b", 120);
    }

    /// A selected long block that is no block any more as drawn (folded into its group with
    /// Ctrl-O, or short enough to show whole at a wider width) is not selected: Enter does
    /// what it does with none (the conversation selected: nothing) and toggles nothing. Nor
    /// is one under a confirmation that takes the whole main pane: Esc does what it does with
    /// none, releasing the conversation.
    #[test]
    fn enter_on_a_block_gone_from_the_conversation_toggles_nothing() {
        // A long tool call, shown with Ctrl-O, then folded.
        let (mut st, mut o) = orch_reply_and_more("hello");
        for ev in tool_calls(&mut o, 0..1, |_| words("argq", 300)) {
            st.apply_event(&ev);
        }
        st.set_focus(None);
        st.handle_key(ctrl('o'));
        draw(&mut st, 120, 30);
        step(&mut st, KeyCode::Up);
        let key_of = st.view.block.clone().unwrap();
        assert!(key_of.contains(":tool:"), "{key_of}");
        draw(&mut st, 120, 30);
        st.handle_key(ctrl('o'));
        draw(&mut st, 120, 30);
        assert_eq!(st.view.block, None);
        let expanded = st.view.long_expanded.clone();
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None);
        assert_eq!(st.view.long_expanded, expanded);
        // A box long only at a narrow width, selected there, then widened.
        let (mut st, _) = orch_reply_and_more(&boxed("c", 8, 12));
        st.set_focus(None);
        draw(&mut st, 50, 30);
        step(&mut st, KeyCode::Up);
        assert!(st.view.block.is_some());
        draw(&mut st, 50, 30);
        draw(&mut st, 120, 30);
        assert!(block_keys(&st).is_empty());
        assert_eq!(st.view.block, None);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None);
        assert!(st.view.long_expanded.is_empty());
        // Selected under a confirmation's banner, which then takes the whole pane.
        let (mut st, _) = orch_reply_and_more(&boxed("d", 30, 3));
        control(&mut st, &[long_confirm_card("cc1", 12)]);
        st.set_focus(None);
        draw(&mut st, 120, 40);
        assert!(st.view.panes.card.is_none());
        step(&mut st, KeyCode::Up);
        assert!(st.view.block.is_some());
        draw(&mut st, 120, 14);
        assert!(st.view.panes.card.is_some());
        assert_eq!(st.view.block, None);
        assert!(st.main_focused());
        st.handle_key(key(KeyCode::Esc));
        assert!(!st.main_focused());
    }

    /// While a line is typed, letters and Enter are the line's (a selected block changes
    /// nothing); a long block's footer says a click opens it, a selected one's that Enter
    /// does; a click on the footer opens it and selects it, the line kept.
    #[test]
    fn typing_keeps_enter_for_the_line() {
        let (mut st, _) = orch_reply_and_more(&boxed("d", 30, 3));
        draw(&mut st, 120, 40);
        st.handle_key(key(KeyCode::Home));
        draw(&mut st, 120, 40);
        let keys = block_keys(&st);
        st.view.block = Some(keys[0].clone());
        for c in "e{}".chars() {
            assert!(st.handle_key(key(KeyCode::Char(c))).is_empty());
        }
        assert_eq!(st.view.input.as_str(), "e{}");
        let rows = drawn_rows(&mut st, 120, 40);
        assert!(
            rows.iter().any(|r| r.contains("· click to expand")),
            "{rows:?}"
        );
        assert!(matches!(
            st.handle_key(key(KeyCode::Enter)).as_slice(),
            [Action::Say { text, .. }] if text == "e{}"
        ));
        assert!(st.view.long_expanded.is_empty());
        // Selected (back at the top: sending followed the bottom), the footer names Enter.
        st.handle_key(key(KeyCode::Home));
        st.view.block = Some(keys[0].clone());
        let rows = drawn_rows(&mut st, 120, 40);
        assert!(
            rows.iter().any(|r| r.contains("· Enter to expand")),
            "{rows:?}"
        );
        // A click on the footer while typing.
        st.view.input.set("half a line");
        let buf = draw(&mut st, 120, 40);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
        let footer = tr
            .links
            .iter()
            .position(|l| l.as_deref() == Some(&format!("toggle:{}", keys[0])))
            .unwrap() as u16
            + tr.area.y;
        let at = |kind| mouse(kind, tr.area.x + 2, footer);
        st.handle_mouse(at(MouseEventKind::Down(MouseButton::Left)), &buf);
        st.handle_mouse(at(MouseEventKind::Up(MouseButton::Left)), &buf);
        assert!(st.view.long_expanded.contains(&keys[0]));
        assert_eq!(st.view.block.as_ref(), Some(&keys[0]));
        assert!(st.main_focused(), "the click selects the conversation");
        assert_eq!(st.view.input.as_str(), "half a line");
    }

    /// Block A selected while reading in thread box B, which then collapses at a narrower
    /// width: `e` on A leaves the view's place in B as it was, and widened again, the view is
    /// back on B's text.
    #[test]
    fn toggling_another_block_keeps_a_place_collapsed_away() {
        let (mut st, mut o) = orch_reply_and_more(&boxed("a", 30, 3));
        st.apply_event(&thread_item(&o.text(&boxed("b", 10, 12))));
        for i in 0..20 {
            st.apply_event(&thread_item(&o.text(&format!("tail-{i}"))));
        }
        draw(&mut st, 120, 30);
        let keys = block_keys(&st);
        assert_eq!(keys.len(), 1, "only A is long at 120 columns: {keys:?}");
        let later: Vec<String> = (6..10).map(|k| format!("b{k}q0")).collect();
        let read = wheel_to_any(&mut st, &later, 120, 30);
        st.view.block = Some(keys[0].clone());
        let narrow = drawn_rows(&mut st, 50, 30);
        assert!(narrow[0].contains("more lines"), "{narrow:?}");
        assert_eq!(st.view.block.as_ref(), Some(&keys[0]));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.long_expanded.contains(&keys[0]));
        draw(&mut st, 50, 30);
        assert_eq!(drawn_rows(&mut st, 120, 30), read);
    }

    /// One long paragraph in a thread box that fits at one width and collapses at a narrower
    /// one: a view near its end goes to the box's footer, not to the last row kept of the
    /// paragraph, and back at the first width it is where it was.
    #[test]
    fn a_view_in_the_hidden_part_of_one_long_line_goes_to_the_footer() {
        let line = words("lq", 150);
        let (mut st, _) =
            orch_reply_and_more(&format!("<thread name=\"alpha\">\n{line}\n</thread>"));
        draw(&mut st, 120, 30);
        assert!(block_keys(&st).is_empty());
        let end: Vec<String> = (130..150).map(|k| format!("lq{k}")).collect();
        let read = wheel_to_any(&mut st, &end, 120, 30);
        let narrow = drawn_rows(&mut st, 50, 30);
        assert!(narrow[0].contains("more lines"), "{narrow:?}");
        assert_eq!(drawn_rows(&mut st, 120, 30), read);
    }

    /// [`orch_reply_and_more`] with the sidebar shown and `t-alpha`'s card selected.
    fn conversation_and_cards(text: &str) -> AppState {
        let (mut st, _) = orch_reply_and_more(text);
        st.view.sidebar_hidden = false;
        st.set_focus(Some("t-alpha".into()));
        st
    }

    /// Shift-← selects the conversation and Shift-→ gives the selection back to the card
    /// selected before. Ctrl-↑/↓ and Alt-↑/↓ scroll the conversation a line, whatever is
    /// selected: up, it stops following and its last row counts what is below; down to
    /// the bottom, it follows again. PgUp, Home and End scroll it as always.
    #[test]
    fn shift_left_selects_the_conversation_and_shift_right_the_card_before() {
        for w in [160, 80] {
            let mut st = conversation_and_cards("hello");
            draw(&mut st, w, 30);
            st.handle_key(shift(KeyCode::Left));
            assert!(st.main_focused(), "{w}");
            assert_eq!(
                st.focused(),
                Some("t-alpha"),
                "the card is kept to return to"
            );
            st.handle_key(mods(KeyCode::Up, KeyModifiers::CONTROL));
            let buf = draw(&mut st, w, 30);
            assert_eq!(st.view.scroll, 1, "{w}");
            // That line, and the row that says so.
            assert!(screen_text(&buf).contains("↓ 2 lines below"), "{w}");
            assert_eq!(
                st.focused(),
                Some("t-alpha"),
                "Ctrl-↑ scrolls, it selects no card"
            );
            for _ in 0..3 {
                st.handle_key(mods(KeyCode::Up, KeyModifiers::ALT));
                draw(&mut st, w, 30);
            }
            assert_eq!(st.view.scroll, 4);
            let read = drawn_rows(&mut st, w, 30);
            // New text below keeps the view where it is.
            let mut o = ThreadScript::new("t-orch");
            o.at(200);
            st.apply_event(&thread_item(&o.text("newer")));
            assert_eq!(drawn_rows(&mut st, w, 30), read, "{w}");
            for i in 0..5 {
                let m = if i % 2 == 0 {
                    KeyModifiers::CONTROL
                } else {
                    KeyModifiers::ALT
                };
                st.handle_key(mods(KeyCode::Down, m));
                draw(&mut st, w, 30);
            }
            assert_eq!(st.view.scroll, 0, "back at the bottom it follows");
            assert!(st.view.anchor.is_none());
            st.handle_key(key(KeyCode::PageUp));
            draw(&mut st, w, 30);
            assert!(st.view.scroll > 0);
            st.handle_key(key(KeyCode::End));
            draw(&mut st, w, 30);
            assert_eq!(st.view.scroll, 0);
            assert!(st.main_focused(), "{w}");
            st.handle_key(shift(KeyCode::Right));
            assert!(!st.main_focused());
            assert_eq!(st.focused(), Some("t-alpha"), "{w}");
            // Shift-↑ and Shift-↓ select cards again.
            st.handle_key(shift(KeyCode::Down));
            assert_ne!(st.focused(), Some("t-alpha"));
            assert_eq!(st.view.scroll, 0);
        }
    }

    /// Ctrl-↑/↓ and Alt-↑/↓ scroll the main pane a line with a card selected too, and with
    /// nothing selected; they are no rows of the input line, which keeps its cursor.
    #[test]
    fn ctrl_and_alt_arrows_scroll_a_line_whatever_is_selected() {
        for focus in [Some("t-beta"), None] {
            let mut st = conversation_and_cards("hello");
            st.set_focus(focus.map(str::to_string));
            st.view.input.set("one\ntwo");
            draw(&mut st, 160, 30);
            let cursor = st.view.input.cursor();
            st.handle_key(mods(KeyCode::Up, KeyModifiers::CONTROL));
            draw(&mut st, 160, 30);
            assert_eq!(st.view.scroll, 1, "{focus:?}");
            st.handle_key(mods(KeyCode::Up, KeyModifiers::ALT));
            draw(&mut st, 160, 30);
            assert_eq!(st.view.scroll, 2, "{focus:?}");
            assert_eq!(st.view.input.cursor(), cursor, "no row of the line");
            st.handle_key(mods(KeyCode::Down, KeyModifiers::ALT));
            st.handle_key(mods(KeyCode::Down, KeyModifiers::CONTROL));
            draw(&mut st, 160, 30);
            assert_eq!(st.view.scroll, 0, "{focus:?}");
            assert_eq!(st.view.input.cursor(), cursor);
            assert_eq!(st.focused(), focus);
            // A plain ↑ is the line's: a row up.
            st.handle_key(key(KeyCode::Up));
            assert_ne!(st.view.input.cursor(), cursor);
            assert_eq!(st.view.scroll, 0);
        }
    }

    /// With the conversation selected, no card is (the card kept to return to is not drawn
    /// selected): Enter on an empty line opens no popup, and `/pin` without a thread says how
    /// to select the cards. A request selected in the conversation (Shift-↓) takes Enter: its
    /// popup answers it. Esc releases the request, then the conversation; Tab selects a card.
    #[test]
    fn with_the_conversation_selected_no_card_is() {
        let mut st = with_orch();
        draw(&mut st, 160, 40);
        st.set_focus(Some("t-beta".into()));
        st.handle_key(shift(KeyCode::Left));
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None);
        assert!(type_line(&mut st, "/pin").is_empty());
        assert_eq!(st.view.status.as_deref(), Some(MAIN_SELECTED));
        assert!(st.view.pins.is_empty());
        st.view.input.clear();
        // Shift-↓ selects a request in the conversation; its popup answers it.
        step(&mut st, KeyCode::Down);
        let id = st.view.request.clone().expect("a request is selected");
        assert!(st.main_focused());
        draw(&mut st, 160, 40);
        assert!(matches!(
            answer_with(&mut st, 1).as_slice(),
            [Action::Respond { attn_id, .. }] if *attn_id == id
        ));
        st.handle_key(key(KeyCode::Esc));
        assert!(st.view.request.is_none());
        assert!(st.main_focused());
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.dive.is_none(), "Enter dives into no card");
        assert_eq!(st.view.action, None);
        st.handle_key(key(KeyCode::Esc));
        assert!(!st.main_focused());
        assert_eq!(st.focused(), None);
        st.handle_key(key(KeyCode::Tab));
        assert!(st.focused().is_some(), "Tab selects a card");
    }

    /// The arrows edit the line whatever is selected; the grid's Shift-arrows move between
    /// tiles; leaving the stack view gives the selection back to the cards.
    #[test]
    fn arrows_edit_the_line_and_shift_arrows_move_in_the_grid() {
        let mut st = conversation_and_cards("hello");
        draw(&mut st, 160, 30);
        st.handle_key(shift(KeyCode::Left));
        for c in "abc".chars() {
            st.handle_key(key(KeyCode::Char(c)));
        }
        st.handle_key(key(KeyCode::Left));
        st.handle_key(key(KeyCode::Left));
        st.handle_key(key(KeyCode::Right));
        st.handle_key(key(KeyCode::Char('X')));
        assert_eq!(st.view.input.as_str(), "abXc");
        // On the line's only row, with no history, ↑ goes to its start.
        st.handle_key(key(KeyCode::Up));
        st.handle_key(key(KeyCode::Char('Y')));
        assert_eq!(st.view.input.as_str(), "YabXc");
        assert_eq!(st.view.scroll, 0, "↑ scrolls nothing");
        assert_eq!(st.focused(), Some("t-alpha"));
        assert!(
            st.main_focused(),
            "the conversation is still the one selected"
        );
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        assert!(!st.view.main_focus);
        let before = st.focused().map(str::to_string);
        st.handle_key(shift(KeyCode::Right));
        assert_ne!(
            st.focused().map(str::to_string),
            before,
            "Shift-→ moves in the grid"
        );
        st.handle_key(shift(KeyCode::Left));
        assert_eq!(st.focused().map(str::to_string), before);
        st.handle_key(ctrl('g'));
        assert!(
            !st.main_focused(),
            "back on the stack view, the cards are selected"
        );
    }

    /// In a dive, Shift-← selects the thread's transcript and Ctrl-↑ scrolls it; Esc (no card
    /// selected) leaves the dive, the conversation still selected.
    #[test]
    fn shift_left_selects_the_transcript_dived_into() {
        let (mut st, _) = alpha_replies(40, |i| format!("line-{i:02}"));
        draw(&mut st, 80, 30);
        st.handle_key(shift(KeyCode::Left));
        // The working cursor's rows at the bottom go once scrolled up: a line of text goes
        // below the view all the same.
        st.handle_key(mods(KeyCode::Up, KeyModifiers::CONTROL));
        let buf = draw(&mut st, 80, 30);
        assert_eq!(st.view.scroll, 1);
        assert!(screen_text(&buf).contains("↓ 2 lines below"));
        st.handle_key(mods(KeyCode::Up, KeyModifiers::CONTROL));
        let buf = draw(&mut st, 80, 30);
        assert_eq!(st.view.scroll, 2);
        assert!(screen_text(&buf).contains("↓ 3 lines below"));
        st.handle_key(key(KeyCode::Esc));
        assert!(st.view.dive.is_none());
        assert!(st.main_focused());
    }

    /// Shift-↑/↓ step between the long blocks with the conversation selected, Enter toggles
    /// the selected one, and Ctrl-↓ scrolls the conversation, a selected block staying
    /// selected while it is drawn.
    #[test]
    fn block_steps_work_with_the_conversation_selected() {
        let mut st = conversation_and_cards(&boxed("a", 30, 3));
        draw(&mut st, 160, 30);
        st.handle_key(shift(KeyCode::Left));
        step(&mut st, KeyCode::Down);
        let block = st.view.block.clone().expect("a block is selected");
        shows_head(&mut st, "a", 160);
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.long_expanded.contains(&block));
        draw(&mut st, 160, 30);
        let top = drawn_rows(&mut st, 160, 30)[0].clone();
        st.handle_key(mods(KeyCode::Down, KeyModifiers::CONTROL));
        let rows = drawn_rows(&mut st, 160, 30);
        assert_ne!(rows[0], top, "Ctrl-↓ scrolled a line");
        assert_eq!(st.view.block.as_ref(), Some(&block));
        st.handle_key(key(KeyCode::Enter));
        assert!(!st.view.long_expanded.contains(&block));
        st.handle_key(key(KeyCode::Esc));
        assert!(st.view.block.is_none());
        assert!(
            st.main_focused(),
            "Esc released the block, the conversation stays"
        );
    }

    /// A click on a card selects the cards; a click in the conversation selects it.
    #[test]
    fn clicks_move_the_selection_between_the_panes() {
        let mut st = conversation_and_cards("hello");
        draw(&mut st, 160, 30);
        let tr = st.view.panes.get(&PaneId::Transcript).unwrap().area;
        let buf = draw(&mut st, 160, 30);
        click_at(&mut st, &buf, tr.x + 2, tr.y + 2);
        assert!(st.main_focused());
        let card = st
            .view
            .panes
            .get(&PaneId::Thread("t-gamma".into()))
            .unwrap()
            .area;
        click_at(&mut st, &buf, card.x + 1, card.y);
        assert!(!st.main_focused());
        assert_eq!(st.focused(), Some("t-gamma"));
    }

    /// Shift-→ gives the selection back to the card drawn selected: a long block or a request
    /// selected in the conversation is not any more, so Enter opens the card's popup and its
    /// answers are the card's. The view stays where it is.
    #[test]
    fn shift_right_releases_what_the_conversation_had_selected() {
        let mut st = conversation_and_cards(&boxed("a", 30, 3));
        draw(&mut st, 160, 30);
        st.handle_key(shift(KeyCode::Left));
        step(&mut st, KeyCode::Down);
        assert!(st.view.block.is_some());
        draw(&mut st, 160, 30);
        // (Its footer's hint changes as it is released: the view is its top row.)
        let top = drawn_rows(&mut st, 160, 30)[0].clone();
        st.handle_key(shift(KeyCode::Right));
        assert!(
            st.view.block.is_none(),
            "no block is selected with the cards"
        );
        assert_eq!(drawn_rows(&mut st, 160, 30)[0], top, "the view stays");
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            st.view.dive.as_deref(),
            Some("t-alpha"),
            "Enter, Enter dives into the card"
        );

        // A request of another thread than the card selected before Shift-←: Shift-→ comes
        // back to that card, not to the request's.
        let mut st = with_orch();
        st.set_focus(Some("t-alpha".into()));
        draw(&mut st, 160, 40);
        st.handle_key(shift(KeyCode::Left));
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request.as_deref(), Some("pm1"), "beta's request");
        assert_eq!(st.focused(), Some("t-alpha"));
        draw(&mut st, 160, 40);
        settled(&mut st);
        assert!(st.selected_attention().is_some_and(|i| i.attn_id == "pm1"));
        st.handle_key(shift(KeyCode::Right));
        assert!(
            st.view.request.is_none(),
            "the request is not selected with the cards"
        );
        assert_eq!(st.focused(), Some("t-alpha"));
        assert!(st.selected_attention().is_some_and(|i| i.attn_id == "rv1"));
    }

    /// The decisions overlay and back keeps what was selected: the conversation here.
    #[test]
    fn the_decisions_overlay_round_trip_keeps_the_selection() {
        let mut st = conversation_and_cards("hello");
        draw(&mut st, 160, 30);
        st.handle_key(shift(KeyCode::Left));
        open_decisions(&mut st);
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.screen, Screen::Stack);
        assert!(st.main_focused());
        assert_eq!(st.focused(), Some("t-alpha"));
    }

    /// A request selected with text after it is drawn whole above the row that says what is
    /// below (the view scrolled up to it): its last row is not under that row, so it counts
    /// as drawn in full, at any height that has room for it and that row; one drawn alone at
    /// the bottom needs no such row. With the sidebar hidden, the conversation is the only
    /// place it is drawn.
    #[test]
    fn a_selected_request_is_whole_above_the_row_saying_what_is_below() {
        for after in [0, 1, 2, 30] {
            for h in 16..40u16 {
                let mut st = api_and_web_waiting();
                st.view.sidebar_hidden = true;
                let mut o = ThreadScript::new("t-orch");
                o.at(50);
                st.apply_event(&request_update(&mut o, "pa"));
                for i in 0..after {
                    st.apply_event(&thread_item(&o.text(&format!("reply {i}"))));
                }
                draw(&mut st, 100, h);
                st.select_request("pa");
                let screen = draw(&mut st, 100, h);
                let text = screen_text(&screen);
                // (Its card is 5 rows: room for it and the row under it.)
                let pane = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
                assert!(pane.area.height >= 7, "{h}");
                assert!(
                    drawn_ids(&st).contains(&"pa"),
                    "{after} after, {h} rows:\n{text}"
                );
            }
        }
    }

    /// The first Ctrl-↑ from a request selected at the bottom of a working conversation
    /// releases it and puts a line of text below the view, past the working cursor's own rows
    /// (which go once it is scrolled), as with no request selected.
    #[test]
    fn ctrl_up_from_a_request_at_the_bottom_scrolls_a_line() {
        let mut st = without_worker_requests(with_orch());
        let mut o = ThreadScript::new("t-orch");
        o.at(100);
        for i in 0..40 {
            st.apply_event(&thread_item(&o.text(&format!("reply-{i}"))));
        }
        // A request raised after them: at the bottom of the conversation.
        let mut b = ThreadScript::new("t-beta");
        b.at(200);
        control(&mut st, &[b.raise("late", Permission)]);
        st.apply_event(&request_update(&mut o, "late"));
        o.at(300);
        control(&mut st, &[o.turn_started("more")]);
        st.transcript.as_mut().unwrap().settle();
        let buf = draw(&mut st, 160, 40);
        assert!(screen_text(&buf).contains('_'), "the orchestrator works");
        st.handle_key(shift(KeyCode::Left));
        step(&mut st, KeyCode::Down);
        assert!(st.view.request.is_some());
        draw(&mut st, 160, 40);
        assert_eq!(st.view.panes.get(&PaneId::Transcript).unwrap().scroll, 0);
        st.handle_key(mods(KeyCode::Up, KeyModifiers::CONTROL));
        assert!(st.view.request.is_none());
        let buf = draw(&mut st, 160, 40);
        assert_eq!(st.view.scroll, 1, "{}", screen_text(&buf));
        // That line, and the row that says so.
        assert!(screen_text(&buf).contains("↓ 2 lines below"));
    }

    /// A request at the bottom of a working conversation, one row shorter than the pane,
    /// holds the view a row up, one of the working cursor's rows still in view: the first
    /// Ctrl-↑ goes past that one too, so a line of text goes below the view.
    #[test]
    fn ctrl_up_from_a_tall_request_at_the_bottom_scrolls_a_line() {
        for lines in 1..60 {
            let mut st = without_worker_requests(with_orch());
            let mut o = ThreadScript::new("t-orch");
            o.at(100);
            for i in 0..40 {
                st.apply_event(&thread_item(&o.text(&format!("reply-{i}"))));
            }
            let mut b = ThreadScript::new("t-beta");
            b.at(200);
            let mut raised = b.raise("tall", Permission);
            if let EventBody::AttentionRaised(p) = &mut raised.body {
                p.summary = (0..lines)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            control(&mut st, &[raised]);
            st.apply_event(&request_update(&mut o, "tall"));
            o.at(300);
            control(&mut st, &[o.turn_started("more")]);
            st.transcript.as_mut().unwrap().settle();
            draw(&mut st, 160, 40);
            st.handle_key(shift(KeyCode::Left));
            step(&mut st, KeyCode::Down);
            draw(&mut st, 160, 40);
            let pane = st.view.panes.get(&PaneId::Transcript).unwrap();
            if pane.scroll != 1 || pane.follow_rows != 2 {
                continue;
            }
            st.handle_key(mods(KeyCode::Up, KeyModifiers::CONTROL));
            let buf = draw(&mut st, 160, 40);
            assert_eq!(st.view.scroll, 1, "{lines}: {}", screen_text(&buf));
            assert!(screen_text(&buf).contains("↓ 2 lines below"));
            return;
        }
        panic!("no request held the view a row up");
    }

    /// A request of another thread selected with the conversation selected stays selected
    /// while typing, when the card the conversation returns to is gone (repaired to none),
    /// and through the decisions overlay; Shift-→ releases it, so the card's own item answers.
    #[test]
    fn a_request_the_conversation_selected_is_its_own() {
        let pick = || {
            let mut st = with_orch();
            st.set_focus(Some("t-alpha".into()));
            draw(&mut st, 160, 40);
            st.handle_key(shift(KeyCode::Left));
            step(&mut st, KeyCode::Down);
            assert_eq!(st.view.request.as_deref(), Some("pm1"));
            st
        };
        // Typing.
        let mut st = pick();
        st.handle_key(key(KeyCode::Char('i')));
        assert_eq!(st.view.input.as_str(), "i");
        assert!(st.selected_request().is_some_and(|i| i.attn_id == "pm1"));
        // The card it returns to deleted.
        let mut st = pick();
        let mut a = ThreadScript::new("t-alpha");
        a.at(500);
        control(
            &mut st,
            &[
                a.event(EventBody::ThreadDeleted(twr_core::event::ThreadDeleted {
                    name: "alpha".into(),
                    ..Default::default()
                })),
            ],
        );
        assert_ne!(st.focused(), Some("t-alpha"), "repaired");
        assert!(st.main_focused());
        assert!(st.selected_request().is_some_and(|i| i.attn_id == "pm1"));
        // The decisions overlay and back: still the request; Shift-→ gives alpha's own item.
        let mut st = pick();
        open_decisions(&mut st);
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.request.as_deref(), Some("pm1"));
        st.handle_key(shift(KeyCode::Right));
        assert!(st.view.request.is_none());
        assert_eq!(st.focused(), Some("t-alpha"));
        draw(&mut st, 160, 40);
        assert!(matches!(
            answer_with(&mut st, 1).as_slice(),
            [Action::Respond { attn_id, .. }] if attn_id == "rv1"
        ));
    }

    /// A press and release at `(x, y)` of `screen`.
    fn click_at(st: &mut AppState, screen: &Buffer, x: u16, y: u16) {
        let left = MouseButton::Left;
        st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), screen);
        st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), screen);
    }

    /// The kinds of the action popup's rows, as it is open now.
    fn row_kinds(st: &AppState) -> Vec<ActionKind> {
        let target = st.view.action.as_ref().expect("the popup").target.clone();
        st.action_rows(&target).iter().map(|r| r.kind).collect()
    }

    /// Enter with nothing selected opens nothing. With a card selected it opens the action
    /// popup where the `/` popup is (right above the input box): the item (badge, summary,
    /// note), its options as numbered rows, then Talk to it (highlighted), Pin, Stop, Park. It
    /// draws the item in full, so the item counts as seen and the popup answers it. A worker
    /// with nothing waiting has no options and Attach at the end.
    #[test]
    fn the_action_popup_shows_the_item_and_what_can_be_done() {
        let mut st = three_threads();
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None, "nothing selected");
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Enter));
        let buf = draw(&mut st, 160, 40);
        let s = screen_text(&buf);
        let pop = st.view.panes.get(&PaneId::Popup).expect("the popup").area;
        let input = st.view.panes.get(&PaneId::Input).unwrap().area;
        assert_eq!(pop.bottom() + 2, input.y, "right above the input box: {s}");
        for part in [
            " beta ",
            " PERMISSION ",
            "Bash: rm -rf target",
            "note: build dir only",
            " 1 allow",
            " 2 deny",
            " talk to beta",
            " pin",
            " stop",
            " park",
            "Enter runs · Esc closes",
        ] {
            assert!(s.contains(part), "{part:?} missing: {s}");
        }
        assert_eq!(
            row_kinds(&st),
            [
                ActionKind::Answer(1),
                ActionKind::Answer(2),
                ActionKind::Talk,
                ActionKind::Pin,
                ActionKind::Stop,
                ActionKind::Park
            ]
        );
        assert_eq!(
            st.view.action.as_ref().unwrap().sel,
            Some(ActionKind::Talk),
            "Talk to it"
        );
        assert!(st
            .view
            .panes
            .items
            .iter()
            .any(|d| d.attn_id == "pm1" && d.area == pop));
        assert_eq!(st.seen_gseq(), st.last_gseq);
        st.handle_key(key(KeyCode::Esc));
        // Nothing waiting: no options, and Attach.
        st.set_focus(Some("t-gamma".into()));
        popup_open(&mut st);
        assert_eq!(
            row_kinds(&st),
            [
                ActionKind::Talk,
                ActionKind::Pin,
                ActionKind::Stop,
                ActionKind::Park,
                ActionKind::Attach
            ]
        );
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert!(st.view.status.as_deref().unwrap().contains("nothing"));
        st.handle_key(key(KeyCode::Esc));
        assert!(run_row(&mut st, ActionKind::Attach).is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("to attach, run in another terminal: twr attach gamma")
        );
    }

    /// The popup's first frame starts the settle time, whatever drew the item before: a digit
    /// right after it opens is refused. A revision change while it is open updates its rows
    /// and starts the settle time again; once read, the digit answers the new revision. The
    /// item resolved, the popup shows what is left of the card's rows.
    #[test]
    fn the_action_popup_settles_from_its_first_frame_and_again_on_a_revision() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        settled(&mut st);
        assert!(st.view.shown.is_some(), "the card was read");
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.shown, None, "opening starts it again");
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("the question just changed; read it and answer again")
        );
        assert!(st.view.action.is_some(), "refused: still open");
        settled(&mut st);
        let mut b = ThreadScript::new("t-beta");
        b.at(40);
        let mut again = b.raise("pm1", Permission);
        if let EventBody::AttentionRaised(p) = &mut again.body {
            p.summary = "Bash: rm -rf src".into();
        }
        control(&mut st, &[again]);
        assert!(st.view.action.is_some(), "it stays on the card");
        let s = screen_text(&draw(&mut st, 160, 40));
        st.note_drawn(Instant::now());
        let pop = st.view.panes.get(&PaneId::Popup).unwrap().area;
        let popup: String = s
            .lines()
            .skip(usize::from(pop.y))
            .take(usize::from(pop.height))
            .collect();
        assert!(popup.contains("rm -rf src"), "{s}");
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        settled(&mut st);
        let now = revision(&st, "pm1");
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: now,
                decision: "allow".into(),
            }]
        );
        // Resolved: the card's own rows are left.
        st.handle_key(key(KeyCode::Enter));
        control(&mut st, &[b.resolve("pm1")]);
        assert!(st.view.action.is_some());
        assert!(row_kinds(&st)
            .iter()
            .all(|k| !matches!(k, ActionKind::Answer(_))));
    }

    /// Letters typed in the popup filter its rows: the others are dimmed and skipped by ↑↓
    /// and Enter, none hidden (every option stays drawn, the item still in full); Backspace
    /// takes a letter back; with no row left, Enter says so. Digits answer whatever the filter.
    #[test]
    fn letters_filter_the_action_popups_rows() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        popup_open(&mut st);
        let sel = |st: &AppState| st.view.action.as_ref().unwrap().sel;
        // allow, deny, talk to beta, pin, stop, park
        typed(&mut st, "p");
        assert_eq!(sel(&st), None, "Talk is left out; several rows have p");
        st.handle_key(key(KeyCode::Down));
        assert_eq!(sel(&st), Some(ActionKind::Pin), "the first row it keeps");
        st.handle_key(key(KeyCode::Down));
        assert_eq!(sel(&st), Some(ActionKind::Stop));
        st.handle_key(key(KeyCode::Down));
        st.handle_key(key(KeyCode::Down));
        assert_eq!(sel(&st), Some(ActionKind::Pin), "around");
        st.handle_key(ctrl('p'));
        assert_eq!(sel(&st), Some(ActionKind::Park));
        typed(&mut st, "a");
        assert_eq!(st.view.action.as_ref().unwrap().filter, "pa");
        let s = screen_text(&draw(&mut st, 160, 40));
        assert!(s.contains(" 1 allow") && s.contains("filter: pa"), "{s}");
        assert!(drawn_ids(&st).contains(&"pm1"));
        st.handle_key(key(KeyCode::Backspace));
        st.handle_key(key(KeyCode::Backspace));
        assert_eq!(st.view.action.as_ref().unwrap().filter, "");
        typed(&mut st, "zz");
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.status.as_deref(), Some("no row has zz"));
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('2'))),
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: revision(&st, "pm1"),
                decision: "deny".into(),
            }]
        );
        assert!(st.view.input.is_empty(), "nothing typed into the line");
    }

    /// Tab, Shift-Tab and the Shift-arrows move the selection with the popup open, and the
    /// popup shows the newly selected card's (its item counted from the popup's next frame);
    /// Shift-← (the conversation, which has no popup) closes it, and so does a card that
    /// goes. Scrolling the main pane closes it too: it covers part of the pane.
    #[test]
    fn the_action_popup_follows_the_selection() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        popup_open(&mut st);
        st.handle_key(key(KeyCode::Tab));
        assert_eq!(st.focused(), Some("t-alpha"));
        assert_eq!(
            st.view.action.as_ref().map(|p| p.target.clone()),
            Some(ActionTarget::Card("t-alpha".into()))
        );
        assert_eq!(st.view.shown, None);
        assert!(
            row_kinds(&st).contains(&ActionKind::Dismiss),
            "alpha's review"
        );
        st.handle_key(shift(KeyCode::Down));
        assert_eq!(
            st.view.action.as_ref().map(|p| p.target.clone()),
            Some(ActionTarget::Card("t-gamma".into()))
        );
        st.handle_key(shift(KeyCode::BackTab));
        assert_eq!(st.focused(), Some("t-alpha"));
        assert!(st.view.action.is_some());
        st.handle_key(shift(KeyCode::Left));
        assert!(st.main_focused());
        assert_eq!(st.view.action, None);
        // A card that goes takes its popup with it.
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(90);
        control(&mut st, &[h.raise("hq1", Question)]);
        st.handle_key(shift(KeyCode::Right));
        st.set_focus(Some(HUB_THREAD.into()));
        popup_open(&mut st);
        control(&mut st, &[h.resolve("hq1")]);
        assert_eq!(st.focused(), None);
        assert_eq!(st.view.action, None);
        // PgUp closes it and scrolls nothing.
        st.set_focus(Some("t-beta".into()));
        popup_open(&mut st);
        let scroll = st.view.scroll;
        st.handle_key(key(KeyCode::PageUp));
        assert_eq!(st.view.action, None);
        assert_eq!(st.view.scroll, scroll);
    }

    /// An item too long for the popup: the popup says so, lists no option and answers
    /// nothing, and the item's detail view opens in the main pane (the stack view's, from the
    /// grid). Read there to its options (PgDn), the popup opened again lists them under a
    /// line saying it was read, and answers after the settle. Esc, releasing the card, closes
    /// the detail view.
    #[test]
    fn an_item_too_long_for_the_popup_is_read_in_the_main_pane() {
        let mut st = three_threads();
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        let mut rv = d.raise("rv9", Review);
        if let EventBody::AttentionRaised(p) = &mut rv.body {
            p.summary = (0..80)
                .map(|i| format!("finding {i:02}: a long review line"))
                .collect::<Vec<_>>()
                .join("\n");
            p.options = vec!["retry".into(), "dismiss".into()];
        }
        control(&mut st, &[d.created("delta"), d.turn_started("review"), rv]);
        st.handle_key(ctrl('g'));
        st.set_focus(Some("t-delta".into()));
        st.handle_key(key(KeyCode::Enter));
        let s = screen_text(&draw(&mut st, 160, 40));
        st.note_drawn(Instant::now());
        assert!(s.contains("too long to show here"), "{s}");
        assert!(!s.contains(" 1 retry"), "no option listed: {s}");
        assert_eq!(
            st.view.screen,
            Screen::Stack,
            "the detail is the main pane's"
        );
        assert_eq!(st.view.detail.as_deref(), Some("t-delta"));
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert_eq!(st.view.status.as_deref(), Some(NOT_ON_SCREEN));
        st.handle_key(key(KeyCode::Esc));
        st.view.status = None;
        let s = screen_text(&draw(&mut st, 160, 40));
        st.note_drawn(Instant::now());
        assert!(s.contains(" delta · review · Esc returns "), "{s}");
        assert!(st
            .view
            .panes
            .card
            .as_ref()
            .is_some_and(|w| w.attn_id == "rv9"));
        while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
            st.handle_key(key(KeyCode::PageDown));
            settled(&mut st);
        }
        assert!(drawn_ids(&st).contains(&"rv9"));
        st.handle_key(key(KeyCode::Enter));
        let s = screen_text(&draw(&mut st, 160, 40));
        st.note_drawn(Instant::now());
        assert!(s.contains("read in full in the main pane"), "{s}");
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty(), "not yet");
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "rv9".into(),
                revision: revision(&st, "rv9"),
                decision: "retry".into(),
            }]
        );
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.focused(), None);
        assert_eq!(st.detail_card(), None);
    }

    /// What the action popup covers is not on screen: an item under it caps `seen_gseq`,
    /// while the item the popup draws in full counts.
    #[test]
    fn seen_gseq_counts_nothing_under_the_action_popup() {
        let mut st = api_and_web_waiting();
        st.handle_key(ctrl('b'));
        draw(&mut st, 100, 20);
        assert!(drawn_ids(&st).contains(&"pa") && drawn_ids(&st).contains(&"pw"));
        assert_eq!(st.seen_gseq(), st.last_gseq);
        let web = st.attention.get("pw").unwrap().first_revision;
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request.as_deref(), Some("pa"));
        st.handle_key(key(KeyCode::Enter));
        let s = screen_text(&draw(&mut st, 100, 20));
        assert!(drawn_ids(&st).contains(&"pa"), "{s}");
        assert!(!drawn_ids(&st).contains(&"pw"), "{s}");
        assert_eq!(st.seen_gseq(), web - 1, "{s}");
    }

    /// Each Esc does the first of: close the action popup; take back the last message (not
    /// there yet: nothing); release a request, the conversation, then the card; leave the
    /// dive. Once nothing is left, Esc does nothing. The text typed stays through all of them.
    #[test]
    fn esc_closes_releases_and_leaves_but_keeps_the_text() {
        let mut st = with_orch();
        draw(&mut st, 160, 40);
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.action.is_some());
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.action, None);
        assert_eq!(st.focused(), Some("t-beta"), "only the popup closed");
        typed(&mut st, "draft");
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request.as_deref(), Some("pm1"));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.request, None);
        assert!(st.main_focused(), "the request first");
        st.handle_key(key(KeyCode::Esc));
        assert!(!st.main_focused());
        assert_eq!(st.focused(), None, "then the conversation");
        st.handle_key(key(KeyCode::Tab));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.focused(), None, "a card");
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.input.as_str(), "draft");
        st.dive("t-gamma");
        st.handle_key(key(KeyCode::Tab));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (st.view.dive.as_deref(), st.focused()),
            (Some("t-gamma"), None)
        );
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None, "then the dive");
        assert_eq!(st.view.input.as_str(), "draft");
    }

    /// `/grid` toggles the grid like Ctrl-G; `/quit` quits like Ctrl-C.
    #[test]
    fn grid_and_quit_commands() {
        let mut st = three_threads();
        assert!(type_line(&mut st, "/grid").is_empty());
        assert_eq!(st.view.screen, Screen::Grid);
        assert!(type_line(&mut st, "/grid").is_empty());
        assert_eq!(st.view.screen, Screen::Stack);
        assert_eq!(type_line(&mut st, "/quit"), [Action::Quit]);
    }

    /// In a dive with nothing selected, Enter on an empty line opens the action popup of the
    /// thread dived into (there is no Talk row: it is the thread talked to), with the same
    /// guards: its first frame starts the settle time, then a digit answers its request. With
    /// the main pane selected Enter opens nothing; with the sidebar hidden, where its card is
    /// not drawn, the popup still draws the item itself.
    #[test]
    fn in_a_dive_with_nothing_selected_enter_opens_the_dived_threads_popup() {
        let mut st = three_threads();
        st.dive("t-beta");
        assert_eq!(st.focused(), None);
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(
            st.view.action.as_ref().map(|p| p.target.clone()),
            Some(ActionTarget::Card("t-beta".into()))
        );
        assert_eq!(
            row_kinds(&st),
            [
                ActionKind::Answer(1),
                ActionKind::Answer(2),
                ActionKind::Pin,
                ActionKind::Stop,
                ActionKind::Park
            ]
        );
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: revision(&st, "pm1"),
                decision: "allow".into(),
            }]
        );
        // The main pane selected: nothing.
        st.handle_key(shift(KeyCode::Left));
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action, None);
        st.handle_key(shift(KeyCode::Right));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (st.view.dive.as_deref(), st.focused()),
            (Some("t-beta"), None)
        );
        // The sidebar hidden: the popup draws the item.
        st.handle_key(ctrl('b'));
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.action.is_some());
        settled(&mut st);
        assert!(drawn_ids(&st).contains(&"pm1"));
        assert_eq!(answer_with_open(&mut st, 2).len(), 1);
    }

    /// Presses digit `n` in the action popup already open.
    fn answer_with_open(st: &mut AppState, n: u32) -> Vec<Action> {
        st.handle_key(key(KeyCode::Char(char::from_digit(n, 10).unwrap())))
    }

    /// The popup never opens with an answer, Stop or Park highlighted. In
    /// a dive, the dived thread's popup has no Talk row, so it opens with none: Enter there
    /// runs nothing and says how to pick; ↑↓ onto an answer and Enter runs it, as its digit
    /// does. The hub's card (no Talk row) opens with none too.
    #[test]
    fn the_action_popup_never_opens_on_an_answer() {
        let mut st = three_threads();
        st.dive("t-beta");
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action.as_ref().unwrap().sel, None);
        settled(&mut st);
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("press an option's number, or pick a row with ↑↓")
        );
        assert!(st.view.action.is_some());
        st.handle_key(key(KeyCode::Down));
        assert_eq!(
            st.view.action.as_ref().unwrap().sel,
            Some(ActionKind::Answer(1))
        );
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: revision(&st, "pm1"),
                decision: "allow".into(),
            }]
        );
        let mut h = ThreadScript::new(HUB_THREAD);
        h.at(90);
        control(&mut st, &[h.raise("hq1", Question)]);
        st.back();
        st.set_focus(Some(HUB_THREAD.into()));
        st.handle_key(key(KeyCode::Enter));
        assert_eq!(st.view.action.as_ref().unwrap().sel, None);
    }

    /// The item resolved under the open popup leaves the highlight on the
    /// row it was on (Talk), never on whatever row took its place (Stop); a highlighted row
    /// that went takes the highlight with it.
    #[test]
    fn the_highlight_follows_its_row_when_the_rows_change() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        popup_open(&mut st);
        assert_eq!(st.view.action.as_ref().unwrap().sel, Some(ActionKind::Talk));
        let mut b = ThreadScript::new("t-beta");
        b.at(90);
        control(&mut st, &[b.resolve("pm1")]);
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty(), "no Stop");
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"), "Talk ran");
        // An answer highlighted, then resolved: no highlight, and Enter runs nothing.
        let mut st = three_threads();
        st.set_focus(Some("t-alpha".into()));
        popup_open(&mut st);
        st.handle_key(key(KeyCode::Up));
        assert_eq!(
            st.view.action.as_ref().unwrap().sel,
            Some(ActionKind::Answer(2))
        );
        let mut a = ThreadScript::new("t-alpha");
        a.at(90);
        control(&mut st, &[a.resolve("rv1")]);
        drawn(&mut st);
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(st.view.dive, None);
        assert_eq!(
            st.view.status.as_deref(),
            Some("press an option's number, or pick a row with ↑↓")
        );
    }

    /// On a short screen the popup shows a window of its rows that keeps
    /// the highlighted one in view, the rows a filter leaves out going first; Enter runs only a
    /// row the last frame drew.
    #[test]
    fn a_short_popup_scrolls_to_the_highlight_and_runs_only_drawn_rows() {
        let mut st = three_threads();
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Enter));
        let rows_drawn = |st: &AppState| st.view.panes.popup_rows.clone().unwrap().1;
        draw(&mut st, 160, 8);
        assert!(rows_drawn(&st).contains(&ActionKind::Talk));
        assert!(!rows_drawn(&st).contains(&ActionKind::Stop));
        // Down to Stop, with no frame drawing it: Enter refuses.
        while st.view.action.as_ref().unwrap().sel != Some(ActionKind::Stop) {
            st.handle_key(key(KeyCode::Down));
        }
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("that row is not on screen: pick one with ↑↓")
        );
        // The next frame follows the highlight.
        draw(&mut st, 160, 8);
        assert!(rows_drawn(&st).contains(&ActionKind::Stop));
        // Filtered: the rows it leaves out give their room to the one it keeps.
        st.handle_key(key(KeyCode::Up));
        st.handle_key(key(KeyCode::Up));
        draw(&mut st, 160, 8);
        typed(&mut st, "par");
        draw(&mut st, 160, 8);
        assert_eq!(rows_drawn(&st), [ActionKind::Park]);
    }

    /// An option with a line break (LF or CRLF) keeps it in the popup, on
    /// rows of its own, and its rows count: the item is drawn in full with all of them.
    #[test]
    fn popup_options_keep_their_line_breaks() {
        for brk in ["\n", "\r\n"] {
            let mut st = three_threads();
            let mut d = ThreadScript::new("t-delta");
            d.at(70);
            let mut q = d.raise("q9", Question);
            if let EventBody::AttentionRaised(p) = &mut q.body {
                p.summary = "How often?".into();
                p.options = vec![format!("run{brk}once"), "never".into()];
            }
            control(&mut st, &[d.created("delta"), d.turn_started("ask"), q]);
            st.set_focus(Some("t-delta".into()));
            st.handle_key(key(KeyCode::Enter));
            let buf = draw(&mut st, 160, 40);
            st.note_drawn(Instant::now());
            let pop = st.view.panes.get(&PaneId::Popup).unwrap().area;
            let rows: Vec<String> = (pop.y..pop.bottom())
                .map(|y| {
                    (pop.x..pop.right())
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                        .trim()
                        .to_string()
                })
                .collect();
            let at = rows.iter().position(|r| r == "1 run").unwrap_or_else(|| {
                panic!("{brk:?}: {rows:#?}");
            });
            assert_eq!(rows[at + 1], "once", "{brk:?}: {rows:#?}");
            assert_eq!(rows[at + 2], "2 never", "{brk:?}: {rows:#?}");
            assert!(drawn_ids(&st).contains(&"q9"), "{brk:?}");
        }
    }

    /// The `/` and `@` popups take Up and Down alone; with Shift they move
    /// the selection, with Ctrl they scroll the main pane a line, as without a popup.
    #[test]
    fn suggestion_popups_leave_modified_arrows_alone() {
        for line in ["/mo", "ask @"] {
            let mut st = conversation_and_cards("hello");
            st.set_focus(None);
            st.view.input.set(line);
            draw(&mut st, 160, 30);
            assert!(st.popup().is_some(), "{line}");
            st.handle_key(shift(KeyCode::Down));
            assert!(st.focused().is_some(), "{line}: Shift-↓ selected a card");
            assert_eq!(st.view.popup_sel, 0, "{line}");
            st.handle_key(mods(KeyCode::Up, KeyModifiers::CONTROL));
            draw(&mut st, 160, 30);
            assert_eq!(st.view.scroll, 1, "{line}: Ctrl-↑ scrolled a line");
            assert_eq!(st.view.popup_sel, 0, "{line}");
            st.handle_key(key(KeyCode::Down));
            assert_eq!(st.view.popup_sel, 1, "{line}: Down is the popup's");
            assert_eq!(st.view.input.as_str(), line);
        }
    }

    /// With the sidebar hidden, a worker's item too long for the popup,
    /// opened from the grid: the stack view keeps the card selected for its detail view, so
    /// the popup stays open on it and the detail view opens.
    #[test]
    fn a_long_item_from_the_grid_opens_its_detail_with_the_sidebar_hidden() {
        let mut st = three_threads();
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        let mut rv = d.raise("rv9", Review);
        if let EventBody::AttentionRaised(p) = &mut rv.body {
            p.summary = (0..80)
                .map(|i| format!("finding {i:02}: a long review line"))
                .collect::<Vec<_>>()
                .join("\n");
            p.options = vec!["retry".into(), "dismiss".into()];
        }
        control(&mut st, &[d.created("delta"), d.turn_started("review"), rv]);
        st.handle_key(ctrl('b'));
        st.handle_key(ctrl('g'));
        assert!(st.view.sidebar_hidden && st.view.screen == Screen::Grid);
        st.set_focus(Some("t-delta".into()));
        st.handle_key(key(KeyCode::Enter));
        drawn(&mut st);
        assert_eq!(st.view.screen, Screen::Stack);
        assert_eq!(st.focused(), Some("t-delta"));
        assert!(st.view.action.is_some(), "the popup stays");
        assert_eq!(st.detail_card(), Some("t-delta"));
        st.handle_key(key(KeyCode::Esc));
        drawn(&mut st);
        assert!(st
            .view
            .panes
            .card
            .as_ref()
            .is_some_and(|w| w.attn_id == "rv9"));
    }

    /// Shift-↑/↓ in a worker's dive step through what the dive draws,
    /// never another worker's request, which only the orchestrator conversation shows.
    #[test]
    fn steps_in_a_dive_skip_requests_it_does_not_draw() {
        let (mut st, _) = alpha_replies(5, |i| format!("line-{i}"));
        assert!(!st.inline_requests().is_empty(), "beta's request is open");
        draw(&mut st, 80, 30);
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request, None);
        step(&mut st, KeyCode::Up);
        assert_eq!(st.view.request, None);
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None, "nothing phantom to release: Esc leaves");
    }

    /// Backspace with no filter changes nothing: it never highlights an
    /// answer that the next Enter would send.
    #[test]
    fn backspace_with_no_filter_highlights_nothing() {
        let mut st = three_threads();
        st.dive("t-beta");
        popup_open(&mut st);
        assert_eq!(st.view.action.as_ref().unwrap().sel, None);
        st.handle_key(key(KeyCode::Backspace));
        assert_eq!(st.view.action.as_ref().unwrap().sel, None);
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
    }

    /// A letter that several rows have highlights none of them, so a stray
    /// letter then Enter answers nothing; a filter only one row matches highlights it, and
    /// Enter runs it.
    #[test]
    fn a_filter_highlights_a_row_only_when_it_is_the_only_match() {
        let mut st = three_threads();
        st.dive("t-beta");
        popup_open(&mut st);
        // allow, deny, pin, stop, park: "a" is in allow and park.
        typed(&mut st, "a");
        assert_eq!(st.view.action.as_ref().unwrap().sel, None);
        assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
        st.handle_key(key(KeyCode::Backspace));
        typed(&mut st, "deny");
        assert_eq!(
            st.view.action.as_ref().unwrap().sel,
            Some(ActionKind::Answer(2))
        );
        settled(&mut st);
        assert_eq!(
            st.handle_key(key(KeyCode::Enter)),
            [Action::Respond {
                attn_id: "pm1".into(),
                revision: revision(&st, "pm1"),
                decision: "deny".into(),
            }]
        );
    }

    /// A question with more options than the popup has rows for, its
    /// detail view read in full (80 x 24, the sidebar hidden): the popup answers it with an
    /// option it draws, never one it doesn't.
    #[test]
    fn a_detail_read_in_full_is_answered_from_a_popup_listing_part_of_its_options() {
        let mut st = without_worker_requests(with_orch());
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        let mut q = d.raise("q20", Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = (0..30)
                .map(|i| format!("detail line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n");
            p.options = (1..=20).map(|n| format!("o{n}")).collect();
        }
        control(&mut st, &[d.created("delta"), d.turn_started("ask"), q]);
        st.handle_key(ctrl('b'));
        let frame = |st: &mut AppState, settle: bool| {
            draw(st, 80, 24);
            st.note_drawn(Instant::now());
            if let Some(s) = st.view.shown.as_mut().filter(|_| settle) {
                s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
            }
        };
        frame(&mut st, false);
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request.as_deref(), Some("q20"));
        frame(&mut st, false);
        assert!(st.view.panes.card.is_some(), "the detail view");
        while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
            st.handle_key(key(KeyCode::PageDown));
            frame(&mut st, false);
        }
        assert!(drawn_ids(&st).contains(&"q20"), "read to its options");
        st.handle_key(key(KeyCode::Enter));
        // The highlight on Talk, the last row: the window shows the end, without option 1.
        assert_eq!(st.view.action.as_ref().unwrap().sel, Some(ActionKind::Talk));
        frame(&mut st, true);
        let listed = st.view.panes.popup_rows.clone().unwrap().1;
        assert!(!listed.contains(&ActionKind::Answer(1)), "{listed:?}");
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("option 1 is not on screen: pick it with ↑↓, then Enter")
        );
        // Down wraps to option 1: drawn now, and answered.
        st.handle_key(key(KeyCode::Down));
        frame(&mut st, true);

        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "q20".into(),
                revision: revision(&st, "q20"),
                decision: "o1".into(),
            }]
        );
    }

    /// A capped request read to the end of its preview in the detail view, then fetched in
    /// full while its popup covers that view: the full text was never on screen, so the popup
    /// does not answer it, and what is typed counts it as unseen.
    #[test]
    fn a_capped_preview_read_to_its_end_is_no_proof_once_its_full_text_comes() {
        let store = twr_core::blob::MemoryBlobStore::new();
        let mut st = without_worker_requests(with_orch());
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        let mut q = d.raise("qc", Question);
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = (0..400)
                .map(|i| format!("detail line {i:03}"))
                .collect::<Vec<_>>()
                .join("\n");
            p.options = vec!["yes".into(), "no".into()];
        }
        control(
            &mut st,
            &[
                d.created("delta"),
                d.turn_started("ask"),
                capped(&q, &store),
            ],
        );
        let item = st.attention.get("qc").unwrap().clone();
        assert!(st.incomplete(&item).is_some());
        st.handle_key(ctrl('b'));
        let frame = |st: &mut AppState| {
            draw(st, 80, 24);
            st.note_drawn(Instant::now());
            if let Some(s) = st.view.shown.as_mut() {
                s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
            }
        };
        frame(&mut st);
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request.as_deref(), Some("qc"));
        frame(&mut st);
        assert!(st.view.panes.card.is_some(), "the detail view");
        while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
            st.handle_key(key(KeyCode::PageDown));
            frame(&mut st);
        }
        assert!(
            st.view.card_view.seen.iter().all(|s| *s),
            "the preview read"
        );
        st.handle_key(key(KeyCode::Enter));
        assert!(st.view.action.is_some());
        frame(&mut st);
        // Its full text comes while the popup covers the detail view.
        let due = st.hydration_due(Instant::now());
        assert_eq!(due.len(), 1);
        let mut full = raise_item(&st, &q);
        full.gseq = Some(item.revision);
        st.hydrated("qc", item.revision, Ok(full), Instant::now());
        let item = st.attention.get("qc").unwrap().clone();
        assert!(st.incomplete(&item).is_none());
        for _ in 0..2 {
            frame(&mut st);
        }
        assert!(st.view.action.is_some());
        assert!(
            st.seen_gseq() < item.first_revision.min(item.revision),
            "unseen: {} {}",
            st.seen_gseq(),
            item.first_revision
        );
        assert!(st.handle_key(key(KeyCode::Char('1'))).is_empty());
        assert!(!drawn_ids(&st).contains(&"qc"), "never read in full");
    }

    /// An option taller than the popup has room for (a narrow screen), its item's detail view
    /// read in full: the popup lists it on a row by its number and first line, and answers
    /// it. Not read, it is not listed.
    #[test]
    fn an_option_taller_than_the_popup_is_listed_short_once_its_detail_was_read() {
        let mut st = without_worker_requests(with_orch());
        let mut d = ThreadScript::new("t-delta");
        d.at(70);
        let mut q = d.raise("qt", Question);
        let tall = (0..40)
            .map(|i| format!("tall option line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        if let EventBody::AttentionRaised(p) = &mut q.body {
            p.summary = (0..30)
                .map(|i| format!("detail line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n");
            p.options = vec![tall.clone(), "no".into()];
        }
        control(&mut st, &[d.created("delta"), d.turn_started("ask"), q]);
        st.handle_key(ctrl('b'));
        let (w, h) = (50, 24);
        let frame = |st: &mut AppState| {
            let buf = draw(st, w, h);
            st.note_drawn(Instant::now());
            if let Some(s) = st.view.shown.as_mut() {
                s.since = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
            }
            screen_text(&buf)
        };
        frame(&mut st);
        step(&mut st, KeyCode::Down);
        assert_eq!(st.view.request.as_deref(), Some("qt"));
        frame(&mut st);
        assert!(st.view.panes.card.is_some(), "the detail view");
        // Not read: option 1 is never drawn, whichever row is picked.
        st.handle_key(key(KeyCode::Enter));
        frame(&mut st);
        for _ in 0..4 {
            st.handle_key(key(KeyCode::Down));
            frame(&mut st);
            let listed = st.view.panes.popup_rows.clone().unwrap().1;
            assert!(!listed.contains(&ActionKind::Answer(1)), "{listed:?}");
        }
        st.handle_key(key(KeyCode::Esc));
        frame(&mut st);
        assert!(st.view.action.is_none());
        while st.view.panes.card.as_ref().is_some_and(|w| w.end < w.rows) {
            st.handle_key(key(KeyCode::PageDown));
            frame(&mut st);
        }
        assert!(st.view.card_view.seen.iter().all(|s| *s), "read in full");
        st.handle_key(key(KeyCode::Enter));
        frame(&mut st);
        // Down to option 1: listed short, and answered.
        while st.view.action.as_ref().unwrap().sel != Some(ActionKind::Answer(1)) {
            st.handle_key(key(KeyCode::Down));
        }
        let screen = frame(&mut st);
        let listed = st.view.panes.popup_rows.clone().unwrap().1;
        assert!(listed.contains(&ActionKind::Answer(1)), "{listed:?}");
        assert!(
            screen.contains("1 tall option line 00… (read in full)"),
            "{screen}"
        );
        assert!(!screen.contains("tall option line 01"), "{screen}");
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'))),
            [Action::Respond {
                attn_id: "qt".into(),
                revision: revision(&st, "qt"),
                decision: tall,
            }]
        );
    }

    // --- taking back a message ---------------------------------------------------------------

    /// `turn.started` of a turn a queued message (`msg_ref`) started.
    fn ref_turn(s: &mut ThreadScript, prompt: &str, msg_ref: i64) -> Event {
        let mut ev = s.turn_started(prompt);
        if let EventBody::TurnStarted(p) = &mut ev.body {
            p.ext = serde_json::json!({"twr": {"ref": msg_ref}})
                .as_object()
                .cloned();
        }
        ev
    }

    /// The script of [`three_threads`]'s `beta` where that left it (turn 1 running), at `at`.
    fn beta_script(at: i64) -> ThreadScript {
        let mut b = ThreadScript::new("t-beta");
        b.created("beta");
        b.turn_started("deploy");
        b.raise("pm1", Permission);
        b.annotate("pm1", "build dir only");
        b.at(at);
        b
    }

    /// Sends `text` from the input line, and gives it the hub's `result` (`say` without a
    /// dive, `thread.send` in one).
    fn send_line(st: &mut AppState, text: &str, result: Value) -> Vec<Action> {
        let sent = type_line(st, text);
        let method = if st.view.dive.is_some() {
            "thread.send"
        } else {
            "say"
        };
        st.apply_result(method, &result);
        sent
    }

    /// The hub took the message back.
    fn taken_back(st: &mut AppState, msg_ref: i64) {
        st.apply_result(
            "message.withdraw",
            &serde_json::json!({"withdrawn": true, "state": "queued", "ref": msg_ref}),
        );
    }

    /// With nothing typed, the first Esc only arms the second and shows the hint in the input
    /// box's title; the second takes the queued message back, and its text comes back into
    /// the empty box. Then there is nothing left to take back.
    #[test]
    fn a_double_esc_takes_back_the_last_message_and_puts_it_back() {
        let mut st = with_orch();
        let sent = send_line(
            &mut st,
            "deploy api",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 7,
                               "turn": null, "pending": 1}),
        );
        assert!(matches!(&sent[..], [Action::Say { text, .. }] if text == "deploy api"));
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(
            st.unsend_hint().as_deref(),
            Some(" Esc or tap again: take back your last message ")
        );
        let screen = screen_text(&draw(&mut st, 120, 30));
        assert!(
            screen.contains("→ orchestrator  Esc or tap again: take back your last message"),
            "{screen}"
        );
        // Only with nothing typed.
        typed(&mut st, "x");
        assert_eq!(st.unsend_hint(), None);
        st.handle_key(key(KeyCode::Backspace));
        assert!(st.unsend_hint().is_some());
        let back = st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            back,
            [Action::Withdraw {
                thread: "t-orch".into(),
                msg_ref: 7
            }]
        );
        assert_eq!(
            crate::action_request(&back[0]),
            Some((
                "message.withdraw",
                serde_json::json!({"thread": "t-orch", "ref": 7})
            ))
        );
        assert_eq!(st.unsend_hint(), None);
        // A third Esc while the hub answers is no second withdrawal.
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        taken_back(&mut st, 7);
        assert_eq!(st.view.input.as_str(), "deploy api");
        assert_eq!(
            st.view.status.as_deref(),
            Some("taken back: your message is in the box again")
        );
        st.view.input.clear();
        assert!(type_line(&mut st, "/unsend").is_empty());
        assert_eq!(st.view.status.as_deref(), Some("nothing to take back"));
    }

    /// The second Esc counts for [`UNSEND_WINDOW`]: after it, the hint is spent and Esc goes
    /// on to its other steps (here: releasing the card) for that message. With text typed
    /// Esc never arms. In a dive the first Esc leaves at once, and the hint, back in the
    /// orchestrator conversation, names the thread.
    #[test]
    fn outside_its_window_the_double_esc_lets_esc_go_on() {
        let mut st = with_orch();
        send_line(
            &mut st,
            "check the logs",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 3,
                               "turn": null, "pending": 1}),
        );
        typed(&mut st, "draft");
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.unsend_hint(), None, "text typed: no hint");
        assert_eq!(st.focused(), None, "Esc released the card");
        st.view.input.clear();
        st.set_focus(Some("t-beta".into()));
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(st.unsend_hint().is_some());
        assert_eq!(st.focused(), Some("t-beta"), "the first Esc only armed");
        let late = Instant::now() - UNSEND_WINDOW - Duration::from_millis(50);
        st.sent.as_mut().unwrap().armed = Some(late);
        assert_eq!(st.unsend_deadline(), Some(late + UNSEND_WINDOW));
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(st.focused(), None, "too late: Esc released the card");
        assert_eq!(st.unsend_hint(), None);
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.focused(), None, "spent: Esc goes on");
        // The timer runs out without a key too.
        let mut b = beta_script(60);
        control(&mut st, &[b.turn_ended(TurnResult::Completed)]);
        st.dive("t-beta");
        send_line(
            &mut st,
            "and the tests",
            serde_json::json!({"thread": "t-beta", "ref": 4, "turn": null, "pending": 1}),
        );
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None, "Esc left the dive at once");
        assert_eq!(
            st.unsend_hint().as_deref(),
            Some(" Esc or tap again: take back your message to beta ")
        );
        st.sent.as_mut().unwrap().armed = Some(late);
        st.expire_unsend(Instant::now());
        assert_eq!(st.unsend_hint(), None);
        assert!(
            st.handle_key(key(KeyCode::Esc)).is_empty(),
            "too late: nothing taken back"
        );
        assert_eq!(st.view.dive, None);
        // `/unsend` still takes it back, from anywhere; its text then waits in the history.
        let back = type_line(&mut st, "/unsend");
        assert_eq!(
            back,
            [Action::Withdraw {
                thread: "t-beta".into(),
                msg_ref: 4
            }]
        );
        taken_back(&mut st, 4);
        assert_eq!(st.view.input.as_str(), "");
        assert_eq!(
            st.view.status.as_deref(),
            Some("taken back: the message is in the history (↑)")
        );
        assert!(st.view.history.contains(&"and the tests".to_string()));
    }

    /// A message whose turn runs is taken back while the turn has not acted: the transcript
    /// keeps its prompt, struck out and labelled "taken back"; with text in the box, the
    /// message stays in the history. A turn that started before the hub's answer came is
    /// followed all the same.
    #[test]
    fn a_running_turn_taken_back_is_struck_out() {
        let mut st = with_orch();
        let mut b = beta_script(60);
        control(&mut st, &[b.turn_ended(TurnResult::Completed)]);
        st.dive("t-beta");
        let start = ref_turn(&mut b, "stop that", 9);
        let sent = type_line(&mut st, "stop that");
        assert!(matches!(&sent[..], [Action::Send { .. }]));
        // The turn starts (both streams) before `thread.send` answers.
        control(&mut st, std::slice::from_ref(&start));
        st.apply_event(&thread_item(&start));
        st.apply_result(
            "thread.send",
            &serde_json::json!({"thread": "t-beta", "ref": 9, "turn": null, "pending": 0}),
        );
        assert_eq!(st.sent.as_ref().unwrap().turn, Some(2));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            st.handle_key(key(KeyCode::Esc)),
            [Action::Withdraw {
                thread: "t-beta".into(),
                msg_ref: 9
            }]
        );
        let withdrawn = b.event(EventBody::TurnWithdrawn(twr_core::event::TurnWithdrawn {
            msg_ref: 9,
            turn: 2,
            ..Default::default()
        }));
        control(&mut st, std::slice::from_ref(&withdrawn));
        st.apply_event(&thread_item(&withdrawn));
        typed(&mut st, "x");
        st.apply_result(
            "message.withdraw",
            &serde_json::json!({"withdrawn": true, "state": "running", "ref": 9, "turn": 2}),
        );
        assert_eq!(st.view.input.as_str(), "x");
        assert_eq!(
            st.view.status.as_deref(),
            Some("taken back: the box wasn't empty, so the message is in the history (↑)")
        );
        let entries = &st.transcript.as_ref().unwrap().entries;
        assert!(entries.iter().any(|e| matches!(e,
            Entry::TurnStart { prompt, withdrawn: true, .. } if prompt == "stop that")));
        let buf = draw(&mut st, 120, 30);
        let screen = screen_text(&buf);
        assert!(screen.contains("taken back"), "{screen}");
        let (x, y) = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buf[(x, y)].symbol() == "s"
                    && (1..9).all(|i| {
                        x + i < buf.area.width
                            && buf[(x + i, y)].symbol() == &"stop that"[i as usize..=i as usize]
                    })
            })
            .expect("the prompt is drawn");
        assert!(buf[(x, y)]
            .modifier
            .contains(ratatui::style::Modifier::CROSSED_OUT));
    }

    /// Too late, the status bar says why: the orchestrator acted (a tool call), the worker
    /// started on it (a request), a short command the hub applied at once, a runtime that
    /// cannot interrupt, the hub's own refusal; and Esc then is no double Esc.
    #[test]
    fn too_late_to_take_back_says_why() {
        let mut st = with_orch();
        let mut o = ThreadScript::new("t-orch");
        o.at(70);
        let start = ref_turn(&mut o, "ship it", 5);
        control(&mut st, std::slice::from_ref(&start));
        send_line(
            &mut st,
            "ship it",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 5, "turn": 1}),
        );
        assert!(st.unsend_check().is_ok());
        st.apply_event(&thread_item(&o.event(EventBody::AgentToolCall(
            AgentToolCall {
                call_id: "c1".into(),
                tool: "mcp__twr__send".into(),
                ..Default::default()
            },
        ))));
        st.set_focus(Some("t-beta".into()));
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            (st.unsend_hint(), st.focused()),
            (None, None),
            "Esc went on"
        );
        type_line(&mut st, "/unsend");
        assert_eq!(
            st.view.status.as_deref(),
            Some("too late to take back: the orchestrator already acted on it")
        );

        send_line(
            &mut st,
            "allow api",
            serde_json::json!({"action": "command", "thread": "t-orch", "applied": ["allow"]}),
        );
        type_line(&mut st, "/unsend");
        assert_eq!(
            st.view.status.as_deref(),
            Some("too late to take back: the hub applied it at once")
        );

        let mut b = beta_script(80);
        control(&mut st, &[b.turn_ended(TurnResult::Completed)]);
        st.dive("t-beta");
        control(&mut st, &[ref_turn(&mut b, "go on", 6)]);
        send_line(
            &mut st,
            "go on",
            serde_json::json!({"thread": "t-beta", "ref": 6, "turn": 2}),
        );
        control(&mut st, &[b.raise("pm2", Permission)]);
        type_line(&mut st, "/unsend");
        assert_eq!(
            st.view.status.as_deref(),
            Some("too late to take back: beta already started on it · /stop beta stops it")
        );

        let mut t = ThreadScript::new("t-term");
        let mut created = t.created("term");
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.runtime = "pty".into();
        }
        control(&mut st, &[created, ref_turn(&mut t, "top", 8)]);
        st.dive("t-term");
        send_line(
            &mut st,
            "top",
            serde_json::json!({"thread": "t-term", "ref": 8, "turn": 1}),
        );
        type_line(&mut st, "/unsend");
        assert_eq!(
            st.view.status.as_deref(),
            Some("can't take back: term's runtime can't be interrupted")
        );

        // What this client could not see, the hub says.
        st.back();
        send_line(
            &mut st,
            "one more",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 12,
                               "turn": null, "pending": 1}),
        );
        assert!(!type_line(&mut st, "/unsend").is_empty());
        st.apply_result(
            "message.withdraw",
            &serde_json::json!({"withdrawn": false, "reason": "ended", "ref": 12}),
        );
        assert_eq!(
            st.view.status.as_deref(),
            Some("too late to take back: the orchestrator already acted on it")
        );
        assert_eq!(st.unsend_check(), Err(NoUnsend::TooLate));
        // A call that failed leaves it as it was.
        send_line(
            &mut st,
            "and another",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 13,
                               "turn": null, "pending": 2}),
        );
        assert!(!type_line(&mut st, "/unsend").is_empty());
        st.apply_result(
            "message.withdraw",
            &serde_json::json!({"failed": true, "ref": 13}),
        );
        assert!(st.unsend_check().is_ok());
    }

    /// Two messages sent before the first answer: the answers come in order, and only the
    /// last one's is the last message's.
    #[test]
    fn only_the_last_answer_is_the_last_messages() {
        let mut st = with_orch();
        type_line(&mut st, "first");
        type_line(&mut st, "second");
        st.apply_result(
            "say",
            &serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 1}),
        );
        assert_eq!(st.unsend_check(), Err(NoUnsend::Nothing));
        st.apply_result("say", &Value::Null);
        assert_eq!(
            st.unsend_check(),
            Err(NoUnsend::Nothing),
            "the second failed"
        );
        type_line(&mut st, "third");
        st.apply_result(
            "say",
            &serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 3}),
        );
        assert_eq!(st.unsend_check(), Ok(("t-orch".to_string(), 3)));
        assert_eq!(st.sent.as_ref().unwrap().text, "third");
    }

    /// A line sent with the `//` escape comes back escaped: Enter sends it again as the
    /// message it was, never runs it as a command.
    #[test]
    fn a_message_taken_back_comes_back_as_typed() {
        let mut st = with_orch();
        let sent = send_line(
            &mut st,
            "//stop beta",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 21,
                               "turn": null, "pending": 1}),
        );
        assert!(matches!(&sent[..], [Action::Say { text, .. }] if text == "/stop beta"));
        assert!(!type_line(&mut st, "/unsend").is_empty());
        taken_back(&mut st, 21);
        assert_eq!(st.view.input.as_str(), "//stop beta");
        let again = st.handle_key(key(KeyCode::Enter));
        assert!(matches!(&again[..], [Action::Say { text, .. }] if text == "/stop beta"));
    }

    /// An armed double Esc is for the thread it was armed in: once the main pane shows
    /// another (`/back`), there is no hint, and Esc takes nothing back.
    #[test]
    fn the_double_esc_is_for_the_thread_shown() {
        let mut st = with_orch();
        let mut b = beta_script(60);
        control(&mut st, &[b.turn_ended(TurnResult::Completed)]);
        st.dive("t-beta");
        send_line(
            &mut st,
            "and the docs",
            serde_json::json!({"thread": "t-beta", "ref": 30, "turn": null, "pending": 1}),
        );
        // A card selected in the dive: Esc would release it, so it arms in the dive.
        st.set_focus(Some("t-alpha".into()));
        assert_eq!(st.esc_does(), EscDoes::Release);
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(st.unsend_hint().is_some());
        assert!(type_line(&mut st, "/back").is_empty());
        assert_eq!(st.main_thread().as_deref(), Some("t-orch"));
        assert_eq!(st.unsend_hint(), None);
        assert!(
            st.handle_key(key(KeyCode::Esc)).is_empty(),
            "nothing taken back"
        );
        assert_eq!(st.sent.as_ref().unwrap().armed, None);
        // Armed back in the orchestrator conversation by the Esc that left the dive, it is
        // that view's: diving in again, Esc takes nothing back (and leaves).
        st.sent.as_mut().unwrap().spent = false;
        st.set_focus(None);
        st.dive("t-beta");
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(st.view.dive, None);
        assert!(st.unsend_hint().is_some());
        assert!(type_line(&mut st, "/open beta").is_empty());
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        assert_eq!(st.unsend_hint(), None);
        assert_eq!(st.sent.as_ref().unwrap().armed, None, "disarmed");
        // In its dive again, Esc is a first Esc: it leaves and arms afresh.
        assert!(
            st.handle_key(key(KeyCode::Esc)).is_empty(),
            "nothing taken back"
        );
        assert_eq!(st.view.dive, None, "the Esc left the dive");
        assert!(st.unsend_hint().is_some());
    }

    /// The second Esc counts only in the view the first left the dive for: after Ctrl-G (the
    /// grid, and back), or another dive and `/back` (the same conversation shown again), Esc
    /// does what it does there and takes nothing back, nor does a tap.
    #[test]
    fn a_take_back_armed_by_leaving_a_dive_is_for_that_view_only() {
        let mut st = with_orch();
        let mut b = beta_script(60);
        control(&mut st, &[b.turn_ended(TurnResult::Completed)]);
        let leave_armed = |st: &mut AppState, msg_ref: i64| {
            st.dive("t-beta");
            st.view.input.clear();
            send_line(
                st,
                "and the man page",
                serde_json::json!({"thread": "t-beta", "ref": msg_ref, "turn": null,
                                   "pending": 1}),
            );
            assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
            assert_eq!(st.view.dive, None);
            assert!(st.unsend_hint().is_some());
        };
        leave_armed(&mut st, 33);
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Grid);
        assert_eq!(st.unsend_hint(), None);
        assert!(!st.take_back_offered());
        assert!(st.tap_unsend(Instant::now()).is_empty());
        assert!(
            st.handle_key(key(KeyCode::Esc)).is_empty(),
            "nothing taken back"
        );
        assert_eq!(st.sent.as_ref().unwrap().armed, None);
        assert_eq!(st.view.dive, None);

        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Stack);
        st.sent = None;
        // To the grid and back: the same view again, but not the one it was armed in.
        leave_armed(&mut st, 35);
        st.handle_key(ctrl('g'));
        st.handle_key(ctrl('g'));
        assert_eq!(st.view.screen, Screen::Stack);
        assert_eq!(st.unsend_hint(), None);
        assert!(
            st.handle_key(key(KeyCode::Esc)).is_empty(),
            "nothing taken back"
        );
        st.sent = None;
        leave_armed(&mut st, 34);
        assert!(type_line(&mut st, "/open gamma").is_empty());
        assert!(type_line(&mut st, "/back").is_empty());
        assert_eq!(st.main_thread().as_deref(), Some("t-orch"));
        assert_eq!(st.unsend_hint(), None);
        assert!(
            st.handle_key(key(KeyCode::Esc)).is_empty(),
            "nothing taken back"
        );
        assert_eq!(st.sent.as_ref().unwrap().armed, None);
    }

    /// Esc leaves a dive at once, even with a message there that can still be taken back: it
    /// never waits for the double Esc's second. That one, in the orchestrator conversation
    /// within [`UNSEND_WINDOW`], takes the message back and returns to the dive, where the
    /// text goes back in the box. A tap on the take-back control does the same.
    #[test]
    fn esc_leaves_a_dive_at_once_and_a_second_takes_the_message_back() {
        let mut st = with_orch();
        let mut b = beta_script(60);
        control(&mut st, &[b.turn_ended(TurnResult::Completed)]);
        st.dive("t-beta");
        send_line(
            &mut st,
            "and the changelog",
            serde_json::json!({"thread": "t-beta", "ref": 31, "turn": null, "pending": 1}),
        );
        assert!(st.take_back_offered());
        assert_eq!(st.esc_does(), EscDoes::Leave);
        // No time passes: the Esc acts on the spot, without a timer.
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(st.view.dive, None, "back in the orchestrator conversation");
        assert_eq!(st.main_thread().as_deref(), Some("t-orch"));
        assert_eq!(
            st.unsend_hint().as_deref(),
            Some(" Esc or tap again: take back your message to beta ")
        );
        let screen = screen_text(&draw(&mut st, 120, 30));
        assert!(
            screen.contains("→ orchestrator  Esc or tap again: take back your message to beta"),
            "{screen}"
        );
        assert!(
            st.take_back_offered(),
            "the take-back control is offered there too"
        );
        let back = st.handle_key(key(KeyCode::Esc));
        assert_eq!(
            back,
            [Action::Withdraw {
                thread: "t-beta".into(),
                msg_ref: 31
            }]
        );
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"), "back in its dive");
        taken_back(&mut st, 31);
        assert_eq!(st.view.input.as_str(), "and the changelog");
        assert_eq!(
            st.view.status.as_deref(),
            Some("taken back: your message is in the box again")
        );

        // A tap on the take-back control, back in the orchestrator conversation.
        st.view.input.clear();
        send_line(
            &mut st,
            "and the readme",
            serde_json::json!({"thread": "t-beta", "ref": 32, "turn": null, "pending": 1}),
        );
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None);
        let back = st.tap_unsend(Instant::now());
        assert_eq!(
            back,
            [Action::Withdraw {
                thread: "t-beta".into(),
                msg_ref: 32
            }]
        );
        assert_eq!(st.view.dive.as_deref(), Some("t-beta"));

        // Nothing to take back: Esc leaves too, and arms nothing.
        st.sent = None;
        st.handle_key(key(KeyCode::Esc));
        assert_eq!(st.view.dive, None);
        assert_eq!(st.unsend_deadline(), None);
    }

    /// A withdrawal's answer is matched by its ref: a message sent while it was on its way
    /// does not lose it, and stays the one Esc and `/unsend` take back.
    #[test]
    fn a_withdrawal_answer_is_matched_by_its_ref() {
        let mut st = with_orch();
        send_line(
            &mut st,
            "first thought",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 40,
                               "turn": null, "pending": 1}),
        );
        assert!(!type_line(&mut st, "/unsend").is_empty());
        send_line(
            &mut st,
            "second thought",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 41,
                               "turn": null, "pending": 1}),
        );
        taken_back(&mut st, 40);
        assert_eq!(st.view.input.as_str(), "first thought");
        assert_eq!(
            st.view.status.as_deref(),
            Some("taken back: your message is in the box again")
        );
        assert_eq!(st.unsend_check(), Ok(("t-orch".to_string(), 41)));
    }

    /// A message too long for the history file, taken back while the box holds a draft, is
    /// the newest history entry all the same (for this session), as the status says.
    #[test]
    fn a_long_message_taken_back_is_in_the_history() {
        let mut st = with_orch();
        let long = "x".repeat(crate::HISTORY_ENTRY_MAX + 10);
        st.view.input.set(&long);
        st.handle_key(key(KeyCode::Enter));
        st.apply_result(
            "say",
            &serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 50,
                                "turn": null, "pending": 1}),
        );
        assert!(
            !st.view.history.contains(&long),
            "too long to keep when sent"
        );
        assert!(!type_line(&mut st, "/unsend").is_empty());
        typed(&mut st, "draft");
        taken_back(&mut st, 50);
        assert_eq!(
            st.view.status.as_deref(),
            Some("taken back: the box wasn't empty, so the message is in the history (↑)")
        );
        assert_eq!(st.view.history.last(), Some(&long));
        assert!(
            !st.view.history_new.contains(&long),
            "not written to the file"
        );
        st.view.input.clear();
        st.handle_key(key(KeyCode::Up));
        assert_eq!(st.view.input.as_str(), long);
    }

    /// A turn that acted before `say` answered with its ref: the message is too late as soon
    /// as the answer binds it, with no hint in between.
    #[test]
    fn a_turn_that_acted_before_the_answer_is_too_late() {
        let mut st = with_orch();
        let mut o = ThreadScript::new("t-orch");
        o.at(70);
        let sent = type_line(&mut st, "ship it");
        assert!(matches!(&sent[..], [Action::Say { .. }]));
        control(&mut st, &[ref_turn(&mut o, "ship it", 60)]);
        st.apply_event(&thread_item(&o.event(EventBody::AgentToolCall(
            AgentToolCall {
                call_id: "c1".into(),
                tool: "mcp__twr__send".into(),
                ..Default::default()
            },
        ))));
        st.apply_result(
            "say",
            &serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 60,
                                "turn": null, "pending": 0}),
        );
        assert_eq!(st.unsend_check(), Err(NoUnsend::TooLate));
        assert!(st.handle_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(st.unsend_hint(), None);
    }

    /// The tracking goes with the view kept for a workspace left: a withdrawal answered after
    /// the switch is applied once it shows again, its text back in the box.
    #[test]
    fn a_withdrawal_answered_after_a_switch_is_applied_on_return() {
        let mut st = with_orch();
        send_line(
            &mut st,
            "wrong workspace",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 70,
                               "turn": null, "pending": 1}),
        );
        assert!(!type_line(&mut st, "/unsend").is_empty());
        let kept = st.take_view();
        let mut back = with_orch();
        back.restore_view(kept);
        back.finish_restore();
        let sent = back
            .sent
            .as_ref()
            .expect("the last message is followed still");
        assert_eq!((sent.msg_ref, sent.withdrawing), (Some(70), true));
        taken_back(&mut back, 70);
        assert_eq!(back.view.input.as_str(), "wrong workspace");
        assert_eq!(back.unsend_check(), Err(NoUnsend::Nothing));
    }

    /// An answer to a withdrawal that comes while the view kept for the workspace is being
    /// restored waits for it: the message goes back into the box of the dive it was sent in,
    /// not into the provisional view.
    #[test]
    fn a_withdrawal_answered_during_the_restore_waits_for_it() {
        let mut st = with_orch();
        let mut b = beta_script(60);
        control(&mut st, &[b.turn_ended(TurnResult::Completed)]);
        st.dive("t-beta");
        send_line(
            &mut st,
            "and the docs",
            serde_json::json!({"thread": "t-beta", "ref": 80, "turn": null, "pending": 1}),
        );
        assert!(!type_line(&mut st, "/unsend").is_empty());
        let kept = st.take_view();
        let mut back = with_orch();
        control(
            &mut back,
            &[beta_script(60).turn_ended(TurnResult::Completed)],
        );
        back.restore_view(kept);
        assert_eq!(
            back.main_thread().as_deref(),
            Some("t-orch"),
            "not restored yet"
        );
        taken_back(&mut back, 80);
        assert_eq!(back.view.input.as_str(), "", "it waits");
        back.finish_restore();
        assert_eq!(back.main_thread().as_deref(), Some("t-beta"));
        assert_eq!(back.view.input.as_str(), "and the docs");
        assert_eq!(
            back.view.status.as_deref(),
            Some("taken back: your message is in the box again")
        );
    }

    /// A message taken back into the history that is too long for the history file stays in
    /// it across a switch and back, after the file's entries.
    #[test]
    fn a_session_only_history_entry_survives_a_switch() {
        let mut st = with_orch();
        let long = "y".repeat(crate::HISTORY_ENTRY_MAX + 1);
        st.keep_taken_back(&long);
        let kept = st.take_view();
        let mut back = with_orch();
        back.restore_view(kept);
        back.load_history(vec!["from the file".into()]);
        assert_eq!(back.view.history, ["from the file".to_string(), long]);
    }

    /// The control stream lost during a restore: the view is restored with what came, and a
    /// withdrawal answered meanwhile is applied (its text back in the box). One that waited
    /// through a switch away before the restore finished goes with the view kept.
    #[test]
    fn answers_waiting_for_a_restore_survive_a_lost_stream_and_a_switch() {
        let mut st = with_orch();
        send_line(
            &mut st,
            "never mind",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 90,
                               "turn": null, "pending": 1}),
        );
        assert!(!type_line(&mut st, "/unsend").is_empty());
        let kept = st.take_view();
        let mut back = with_orch();
        back.restore_view(kept);
        back.catch_up = Some(back.last_gseq + 100);
        taken_back(&mut back, 90);
        assert_eq!(back.view.input.as_str(), "", "it waits for the restore");
        // Switched away before the replay ended, then back.
        let kept = back.take_view();
        let mut again = with_orch();
        again.restore_view(kept);
        again.catch_up = Some(again.last_gseq + 100);
        again.control_ended();
        assert_eq!(again.catch_up, None);
        assert_eq!(again.view.input.as_str(), "never mind");
    }

    /// What taking back a message did stays in the status bar for its time when the turn its
    /// interrupt ends ends at once (or any turn starts or ends meanwhile): only time clears
    /// it. A message's ordinary transient status still goes when the orchestrator answers.
    #[test]
    fn a_take_backs_outcome_stays_when_the_turn_ends() {
        let mut st = with_orch();
        let mut o = ThreadScript::new("t-orch");
        o.at(70);
        let start = ref_turn(&mut o, "ship it", 5);
        control(&mut st, std::slice::from_ref(&start));
        send_line(
            &mut st,
            "ship it",
            serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 5, "turn": 1}),
        );
        assert!(!type_line(&mut st, "/unsend").is_empty());
        taken_back(&mut st, 5);
        let said = Some("taken back: your message is in the box again");
        assert_eq!(st.view.status.as_deref(), said);
        control(&mut st, &[o.turn_ended(TurnResult::Interrupted)]);
        st.apply_event(&thread_item(&o.text("ok")));
        control(&mut st, &[o.turn_started("next")]);
        assert_eq!(st.view.status.as_deref(), said, "still there");
        st.expire_status(Instant::now() + TRANSIENT_STATUS);
        assert_eq!(st.view.status, None, "gone at its time");
        // An ordinary one still goes when the orchestrator answers.
        st.set_status("sent to the orchestrator".into(), true, Instant::now());
        control(&mut st, &[o.turn_ended(TurnResult::Completed)]);
        assert_eq!(st.view.status, None);
    }

    /// Mouse and touch: a click (a tap, on a phone) reaches every common action, and never
    /// answers an item that was not drawn in full and read.
    mod touch {
        use super::*;
        use crate::select::Target;

        /// Draws at `w` x `h` as the IO loop does, noting what it showed now.
        fn frame(st: &mut AppState, w: u16, h: u16) -> Buffer {
            crate::ui::draw_offscreen(st, w, h, Instant::now())
        }

        /// The middle of the area where the last frame drew control `t`.
        fn spot(st: &AppState, t: &Target) -> (u16, u16) {
            let (r, _) = st
                .view
                .panes
                .targets
                .iter()
                .rev()
                .find(|(_, x)| x == t)
                .unwrap_or_else(|| panic!("no {t:?} in {:?}", st.view.panes.targets));
            (r.x + r.width / 2, r.y + r.height / 2)
        }

        /// Whether the last frame drew control `t`.
        fn drawn_control(st: &AppState, t: &Target) -> bool {
            st.view.panes.targets.iter().any(|(_, x)| x == t)
        }

        /// A tap at `(x, y)` of a `w` x `h` screen: the button down and up, a frame drawn
        /// before each and after, as the IO loop draws.
        fn tap_at(st: &mut AppState, w: u16, h: u16, x: u16, y: u16) -> Vec<Action> {
            let left = MouseButton::Left;
            let screen = frame(st, w, h);
            let mut out = st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
            let screen = frame(st, w, h);
            out.extend(st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen));
            frame(st, w, h);
            out
        }

        /// A tap on control `t` as a `w` x `h` frame draws it.
        fn tap(st: &mut AppState, w: u16, h: u16, t: Target) -> Vec<Action> {
            frame(st, w, h);
            let (x, y) = spot(st, &t);
            tap_at(st, w, h, x, y)
        }

        /// A tap on the pane `id` as a `w` x `h` frame draws it (a card, a list's row).
        fn tap_pane(st: &mut AppState, w: u16, h: u16, id: PaneId) -> Vec<Action> {
            frame(st, w, h);
            let a = st
                .view
                .panes
                .get(&id)
                .unwrap_or_else(|| panic!("no {id:?}"))
                .area;
            tap_at(st, w, h, a.x + a.width / 2, a.y)
        }

        /// What is on screen has been there long enough to be read.
        fn read(st: &mut AppState) {
            let then = Instant::now() - CARD_SETTLE - Duration::from_millis(50);
            if let Some(s) = st.view.shown.as_mut() {
                s.since = then;
            }
            if let Some((_, since)) = st.view.confirm_seen.as_mut() {
                *since = then;
            }
        }

        fn wheel_at(st: &mut AppState, w: u16, h: u16, x: u16, y: u16, up: bool) {
            let screen = frame(st, w, h);
            let kind = if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            };
            st.handle_mouse(mouse(kind, x, y), &screen);
            frame(st, w, h);
        }

        fn compact() -> AppState {
            let mut st = three_threads();
            st.view.compact_width = 60;
            st
        }

        /// Swipes (the wheel, as a phone's terminal sends a swipe) down the list of cards
        /// until card `id` is on screen.
        fn swipe_to(st: &mut AppState, w: u16, h: u16, id: &str) {
            for _ in 0..20 {
                frame(st, w, h);
                if st.view.panes.get(&PaneId::Thread(id.into())).is_some() {
                    return;
                }
                let list = st.view.panes.list.expect("a list of cards").area;
                wheel_at(st, w, h, list.x + 1, list.y + 1, false);
            }
            panic!("no card {id} after swiping");
        }

        fn responds(actions: &[Action]) -> bool {
            actions.iter().any(|a| matches!(a, Action::Respond { .. }))
        }

        /// On a phone, the tab bar switches screens: every cell of its rows (3 on a tall
        /// screen, 2 at 40x20, 1 on a short one) is the tab of the third of the width it is
        /// in, the gap after a tab included, and a tap anywhere in it shows that screen.
        #[test]
        fn the_compact_tab_bar_switches_screens_by_thirds() {
            for (w, h, rows) in [(40u16, 20u16, 2u16), (40, 30, 3), (40, 12, 1), (30, 20, 2)] {
                let mut st = compact();
                frame(&mut st, w, h);
                assert!(st.compact(), "{w}x{h}");
                let shares = ratatui::layout::Layout::horizontal(
                    [ratatui::layout::Constraint::Ratio(1, 3); 3],
                )
                .split(ratatui::layout::Rect::new(0, 0, w, 1));
                let panes = [CompactPane::Chat, CompactPane::Needs, CompactPane::Threads];
                for y in 0..rows {
                    for x in 0..w {
                        let k = shares
                            .iter()
                            .position(|r| r.x <= x && x < r.right())
                            .unwrap();
                        assert_eq!(
                            st.view.panes.target_at(x, y),
                            Some(&Target::Screen(panes[k])),
                            "{w}x{h}: {x},{y}"
                        );
                    }
                }
                assert!(
                    !matches!(st.view.panes.target_at(0, rows), Some(Target::Screen(_))),
                    "{w}x{h}: the bar is {rows} rows"
                );
                // A tap on a tab's corners and its gap: its bottom row, its last cell.
                for (k, pane) in panes.iter().enumerate() {
                    let r = shares[k];
                    tap_at(&mut st, w, h, r.x, rows - 1);
                    assert_eq!(st.view.compact_pane, *pane, "{w}x{h} {k}");
                    let other = panes[(k + 1) % 3];
                    tap(&mut st, w, h, Target::Screen(other));
                    assert_eq!(st.view.compact_pane, other);
                    tap_at(&mut st, w, h, r.right() - 1, 0);
                    assert_eq!(st.view.compact_pane, *pane, "{w}x{h} {k}: its last cell");
                }
            }
        }

        /// On the compact Threads screen the wheel (a swipe) scrolls the cards a card at a
        /// time and leaves the selection where it is; a tap on a card's options only selects
        /// the card (nothing is answered), and a tap on the selected card opens its popup.
        #[test]
        fn compact_cards_scroll_with_the_wheel_and_select_with_taps() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            type_line(&mut st, "/t");
            let first = |st: &AppState| st.view.panes.list_first.clone();
            frame(&mut st, 40, 20);
            assert_eq!(first(&st).as_deref(), Some("t-beta"));
            let focus = st.focused().map(str::to_string);
            let list = st.view.panes.list.unwrap().area;
            wheel_at(&mut st, 40, 20, list.x + 1, list.y + 1, false);
            assert_eq!(first(&st).as_deref(), Some("t-alpha"), "a card down");
            assert_eq!(
                st.focused().map(str::to_string),
                focus,
                "the selection stays"
            );
            wheel_at(&mut st, 40, 20, list.x + 1, list.y + 1, false);
            assert_eq!(first(&st).as_deref(), Some("t-gamma"));
            wheel_at(&mut st, 40, 20, list.x + 1, list.y + 1, true);
            assert_eq!(first(&st).as_deref(), Some("t-alpha"), "and back up");
            // alpha's card: a tap on its options' row selects it, answers nothing.
            frame(&mut st, 40, 20);
            let a = st
                .view
                .panes
                .get(&PaneId::Thread("t-alpha".into()))
                .unwrap()
                .area;
            let screen = frame(&mut st, 40, 20);
            let options = (a.y..a.bottom())
                .find(|&y| {
                    (a.x..a.right())
                        .map(|x| screen[(x, y)].symbol())
                        .collect::<String>()
                        .contains("1 retry")
                })
                .expect("its options");
            let actions = tap_at(&mut st, 40, 20, a.x + 2, options);
            assert!(!responds(&actions), "{actions:?}");
            assert_eq!(st.focused(), Some("t-alpha"));
            assert!(st.view.action.is_none(), "a first tap only selects");
            tap_at(&mut st, 40, 20, a.x + 2, options);
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Card("t-alpha".into()))
            );
        }

        /// On the Needs you list a tap selects a row, and on the Threads screen a card; a tap
        /// on the selected one opens its popup, however long after (a double tap does both);
        /// Talk to it, picked as the popup opens, dives in with one tap.
        #[test]
        fn a_compact_row_or_card_is_selected_then_opened_with_taps() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            type_line(&mut st, "/n");
            assert_eq!(st.focused(), Some("t-beta"), "the most urgent first");
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-alpha".into()));
            assert_eq!(st.focused(), Some("t-alpha"));
            assert!(st.view.action.is_none(), "a first tap only selects");
            // Slow: no double-click timing on a list.
            st.view.last_click = None;
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-alpha".into()));
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Card("t-alpha".into()))
            );
            // Threads: gamma's card, swiped to, then its popup, then Talk to it.
            tap(&mut st, 40, 20, Target::Screen(CompactPane::Threads));
            swipe_to(&mut st, 40, 20, "t-gamma");
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-gamma".into()));
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-gamma".into()));
            assert_eq!(
                st.action_popup().and_then(|p| p.sel),
                Some(ActionKind::Talk)
            );
            tap(&mut st, 40, 20, Target::Row(ActionKind::Talk));
            assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
            assert_eq!(st.view.compact_pane, CompactPane::Chat);
            // The dive's header takes you back.
            tap(&mut st, 40, 20, Target::Back);
            assert_eq!(st.view.dive, None);
        }

        /// A tap on an option never answers at once: the first picks the row, and only a tap
        /// on the row it picked, with the item on screen in full long enough to be read when
        /// it picked it, answers. Taps before that answer nothing, however many.
        #[test]
        fn a_tap_never_answers_an_item_too_early() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            type_line(&mut st, "/n");
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-beta".into()));
            assert!(
                st.action_popup().is_some(),
                "beta was selected: the popup opens"
            );
            assert!(st.view.shown.is_some(), "drawn in full");
            for _ in 0..3 {
                let out = tap(&mut st, 40, 20, Target::Row(ActionKind::Answer(1)));
                assert!(!responds(&out), "{out:?}");
            }
            assert_eq!(
                st.view.status.as_deref(),
                Some("just shown: read it, then tap again")
            );
            assert_eq!(
                st.action_popup().and_then(|p| p.sel),
                Some(ActionKind::Answer(1))
            );
            read(&mut st);
            // Read: this tap arms it, the next answers.
            assert!(!responds(&tap(
                &mut st,
                40,
                20,
                Target::Row(ActionKind::Answer(1))
            )));
            assert!(st.view.status.as_deref().unwrap().contains("again"));
            let out = tap(&mut st, 40, 20, Target::Row(ActionKind::Answer(1)));
            assert!(
                matches!(&out[..], [Action::Respond { attn_id, decision, .. }]
                    if attn_id == "pm1" && decision == "allow"),
                "{out:?}"
            );
        }

        /// A row armed on one item answers nothing once the item changed under it: the
        /// replacement has to be read, and picked again.
        #[test]
        fn a_tap_armed_on_one_item_never_answers_its_replacement() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            type_line(&mut st, "/n");
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-beta".into()));
            read(&mut st);
            tap(&mut st, 40, 20, Target::Row(ActionKind::Answer(1)));
            // pm1 is raised again with other text: the popup shows it, its rows the same.
            let before = revision(&st, "pm1");
            let mut b = ThreadScript::new("t-beta");
            b.at(40);
            let mut again = b.raise("pm1", Permission);
            if let EventBody::AttentionRaised(p) = &mut again.body {
                p.summary = "Bash: git push --force".into();
            }
            control(&mut st, &[again]);
            frame(&mut st, 40, 20);
            assert!(st.action_popup().is_some());
            assert_ne!(revision(&st, "pm1"), before);
            let out = tap(&mut st, 40, 20, Target::Row(ActionKind::Answer(1)));
            assert!(!responds(&out), "armed on pm1: {out:?}");
            read(&mut st);
            assert!(!responds(&tap(
                &mut st,
                40,
                20,
                Target::Row(ActionKind::Answer(1))
            )));
            let out = tap(&mut st, 40, 20, Target::Row(ActionKind::Answer(1)));
            assert!(
                matches!(&out[..], [Action::Respond { attn_id, revision: r, .. }]
                    if attn_id == "pm1" && *r == revision(&st, "pm1")),
                "{out:?}"
            );
        }

        /// An item the popup cannot draw in full is never answered by a tap: too long for
        /// it, its options are not listed (no row to tap). An option drawn on a card is only
        /// text: a tap on it selects the card.
        #[test]
        fn a_tap_never_answers_an_item_only_partly_drawn() {
            // Too long: no option to tap.
            let mut st = compact();
            let mut d = ThreadScript::new("t-delta");
            d.at(70);
            let mut q = d.raise("q1", Question);
            if let EventBody::AttentionRaised(p) = &mut q.body {
                p.summary = (0..40)
                    .map(|i| format!("line {i} of a long question"))
                    .collect::<Vec<_>>()
                    .join("\n");
                p.options = vec!["yes".into(), "no".into()];
            }
            control(&mut st, &[d.created("delta"), d.turn_started("ask"), q]);
            frame(&mut st, 40, 20);
            type_line(&mut st, "/n");
            st.set_focus(Some("t-delta".into()));
            st.handle_key(key(KeyCode::Enter));
            frame(&mut st, 40, 20);
            read(&mut st);
            let screen = screen_text(&frame(&mut st, 40, 20));
            assert!(st.view.action.is_some());
            assert!(screen.contains("too long to show here"), "{screen}");
            assert!(
                !st.view
                    .panes
                    .targets
                    .iter()
                    .any(|(_, t)| matches!(t, Target::Row(ActionKind::Answer(_)))),
                "{:?}",
                st.view.panes.targets
            );
            // Its rows are there, but no option: a tap on any answers nothing.
            for kind in [ActionKind::Talk, ActionKind::Pin] {
                assert!(drawn_control(&st, &Target::Row(kind)));
            }
            // An option drawn on a card is text: a tap there selects the card, twice opens its
            // popup, and answers nothing.
            let mut st = three_threads();
            let buf = frame(&mut st, 160, 40);
            let card = st
                .view
                .panes
                .get(&PaneId::Thread("t-beta".into()))
                .unwrap()
                .area;
            let row = (card.y..card.bottom())
                .find(|&y| {
                    (card.x..card.right())
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                        .contains("allow")
                })
                .expect("the option on the card");
            for _ in 0..3 {
                read(&mut st);
                st.view.last_click = None;
                let out = tap_at(&mut st, 160, 40, card.x + 2, row);
                assert!(!responds(&out), "{out:?}");
            }
            assert_eq!(st.focused(), Some("t-beta"));
        }

        /// A press on one control and a release where the frame drawn in between put another
        /// one runs neither; a finger's one-cell slide is still a tap.
        #[test]
        fn a_tap_runs_only_the_control_it_went_down_on() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            let (x, y) = spot(&st, &Target::Screen(CompactPane::Needs));
            let left = MouseButton::Left;
            let screen = frame(&mut st, 40, 20);
            st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
            // Something else is drawn there now.
            st.view.panes.targets = vec![(ratatui::layout::Rect::new(0, y, 40, 1), Target::Back)];
            st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen);
            assert_eq!(st.view.compact_pane, CompactPane::Chat);
            // A slide of a cell: a tap.
            let screen = frame(&mut st, 40, 20);
            st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
            st.handle_mouse(mouse(MouseEventKind::Drag(left), x + 1, y), &screen);
            let out = st.handle_mouse(mouse(MouseEventKind::Up(left), x + 1, y), &screen);
            assert!(out.is_empty(), "nothing copied: {out:?}");
            assert_eq!(st.view.compact_pane, CompactPane::Needs);
        }

        /// In the stack view a click selects a card and a click on the selected card opens its
        /// popup; a quick double click still dives in. A request in the conversation: a click
        /// selects it, a click on it again opens its popup.
        #[test]
        fn a_click_on_the_selected_card_or_request_opens_its_popup() {
            let mut st = three_threads();
            tap_pane(&mut st, 160, 40, PaneId::Thread("t-beta".into()));
            assert_eq!(st.focused(), Some("t-beta"));
            assert!(st.view.action.is_none());
            st.view.last_click = None;
            tap_pane(&mut st, 160, 40, PaneId::Thread("t-beta".into()));
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Card("t-beta".into()))
            );
            // Its ✕ closes it.
            tap(&mut st, 160, 40, Target::Close);
            assert!(st.view.action.is_none());
            // A double click dives in.
            st.view.last_click = None;
            tap_pane(&mut st, 160, 40, PaneId::Thread("t-gamma".into()));
            tap_pane(&mut st, 160, 40, PaneId::Thread("t-gamma".into()));
            assert_eq!(st.view.dive.as_deref(), Some("t-gamma"));
            tap(&mut st, 160, 40, Target::Back);
            assert_eq!(st.view.dive, None);
            // beta's request in the conversation.
            let buf = frame(&mut st, 160, 40);
            let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
            let row = (tr.area.y..tr.area.bottom())
                .find(|&y| tr.link(y) == Some("request:pm1"))
                .expect("the request is drawn");
            drop(buf);
            tap_at(&mut st, 160, 40, tr.area.x + 2, row);
            assert_eq!(st.view.request.as_deref(), Some("pm1"));
            assert!(st.view.action.is_none());
            let tr = st.view.panes.get(&PaneId::Transcript).unwrap().clone();
            let row = (tr.area.y..tr.area.bottom())
                .find(|&y| tr.link(y) == Some("request:pm1"))
                .unwrap();
            tap_at(&mut st, 160, 40, tr.area.x + 2, row);
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Request("pm1".into()))
            );
        }

        /// The help and the trash close with their ✕ or a tap outside; the `/` popup's rows
        /// complete with a tap and its ✕ closes it; the wheel over a popup moves its highlight.
        #[test]
        fn overlays_and_popups_close_and_pick_with_taps() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            st.handle_key(key(KeyCode::Char('?')));
            tap(&mut st, 40, 20, Target::Close);
            assert!(!st.view.help);
            // A tap outside the trash (the screen line, above it) closes it, and switches nothing.
            st.view.trash = Some(serde_json::json!({"items": []}));
            frame(&mut st, 40, 20);
            assert!(drawn_control(&st, &Target::Close));
            assert!(
                !drawn_control(&st, &Target::Screen(CompactPane::Needs)),
                "covered"
            );
            tap_at(&mut st, 40, 20, 39, 19);
            assert!(st.view.trash.is_none());
            // The `/` popup: the wheel moves its highlight, a tap completes a row.
            typed(&mut st, "/gr");
            frame(&mut st, 40, 20);
            assert!(st.popup().is_some());
            tap(&mut st, 40, 20, Target::Suggestion("/grid".into()));
            assert_eq!(st.view.input.as_str(), "/grid");
            st.view.input.set("/");
            frame(&mut st, 40, 20);
            let pop = st.view.panes.get(&PaneId::Popup).unwrap().area;
            wheel_at(&mut st, 40, 20, pop.x + 1, pop.y + 1, false);
            assert_eq!(st.view.popup_sel, 1);
            tap(&mut st, 40, 20, Target::Close);
            assert!(st.popup().is_none());
            // The action popup: the wheel moves its highlight, as ↓ does.
            st.view.input.clear();
            type_line(&mut st, "/t");
            swipe_to(&mut st, 40, 20, "t-gamma");
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-gamma".into()));
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-gamma".into()));
            let pop = st.view.panes.get(&PaneId::Popup).unwrap().area;
            wheel_at(&mut st, 40, 20, pop.x + 1, pop.bottom() - 1, false);
            assert_eq!(st.action_popup().and_then(|p| p.sel), Some(ActionKind::Pin));
        }

        /// The question waiting for `y`: `yes` runs it only once it has been on screen long
        /// enough to read, `no` cancels it, and a tap anywhere else cancels it too.
        #[test]
        fn a_question_waiting_for_y_is_answered_with_taps() {
            let mut st = three_threads();
            st.set_focus(Some("t-gamma".into()));
            let w = (120, 30);
            frame(&mut st, w.0, w.1);
            run_row(&mut st, ActionKind::Park);
            assert!(st.view.confirm.is_some());
            frame(&mut st, w.0, w.1);
            assert!(tap(&mut st, w.0, w.1, Target::Yes).is_empty(), "too early");
            assert!(st.view.confirm.is_some());
            read(&mut st);
            let out = tap(&mut st, w.0, w.1, Target::Yes);
            assert!(matches!(&out[..], [Action::Park { .. }]), "{out:?}");
            // No cancels.
            run_row(&mut st, ActionKind::Park);
            tap(&mut st, w.0, w.1, Target::No);
            assert!(st.view.confirm.is_none());
            assert_eq!(st.view.status.as_deref(), Some("cancelled"));
            // So does a tap elsewhere (a card), selecting nothing.
            run_row(&mut st, ActionKind::Park);
            let focus = st.focused().map(str::to_string);
            tap_pane(&mut st, w.0, w.1, PaneId::Thread("t-alpha".into()));
            assert!(st.view.confirm.is_none());
            assert_eq!(st.focused().map(str::to_string), focus);
        }

        /// The status bar's notes do what their keys do; a tap on a notice clears it; the
        /// grid's header goes back to the stack; the main pane's last row, scrolled up,
        /// follows the bottom again: all of it, and nothing above it.
        #[test]
        fn the_status_bar_and_headers_have_controls() {
            let mut st = three_threads();
            st.handle_key(ctrl('b'));
            frame(&mut st, 120, 30);
            tap(&mut st, 120, 30, Target::Sidebar);
            assert!(!st.view.sidebar_hidden);
            st.handle_key(ctrl('f'));
            st.view.status = None;
            tap(&mut st, 120, 30, Target::FocusOff);
            assert!(!st.view.focus_mode);
            tap(&mut st, 120, 30, Target::Notice);
            assert_eq!(st.view.status, None);
            st.handle_key(ctrl('g'));
            tap(&mut st, 120, 30, Target::Stack);
            assert_eq!(st.view.screen, Screen::Stack);
            // A long conversation scrolled up.
            let mut st = three_threads();
            let mut o = ThreadScript::new("t-orch");
            o.at(40);
            let evs = orch_events(&mut o, "t-orch");
            control(&mut st, &evs);
            for i in 0..30 {
                st.apply_event(&thread_item(&o.text(&format!("reply {i}"))));
            }
            frame(&mut st, 120, 30);
            st.handle_key(key(KeyCode::PageUp));
            frame(&mut st, 120, 30);
            assert!(st.view.scroll > 0);
            let (_, y) = spot(&st, &Target::Follow);
            let tr = st.view.panes.get(&PaneId::Transcript).unwrap().area;
            assert_eq!(y, tr.bottom(), "the row under the transcript's");
            for x in tr.x..tr.right() {
                assert_eq!(st.view.panes.target_at(x, y), Some(&Target::Follow), "{x}");
                assert_ne!(
                    st.view.panes.target_at(x, y - 1),
                    Some(&Target::Follow),
                    "{x}"
                );
            }
            tap(&mut st, 120, 30, Target::Follow);
            assert_eq!(st.view.scroll, 0);
            assert!(
                !drawn_control(&st, &Target::Follow),
                "at the bottom: no row for it"
            );
        }

        /// The input box's top border takes back the message just sent: a first tap arms it
        /// (the title says so), a second within the window takes it back.
        #[test]
        fn two_taps_take_back_the_last_message() {
            let mut st = with_orch();
            send_line(
                &mut st,
                "deploy api",
                serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 7,
                                   "turn": null, "pending": 1}),
            );
            let screen = screen_text(&frame(&mut st, 120, 30));
            assert!(screen.contains("↶ take back"), "{screen}");
            assert!(tap(&mut st, 120, 30, Target::TakeBack).is_empty());
            assert!(st.unsend_hint().is_some());
            assert_eq!(
                tap(&mut st, 120, 30, Target::TakeBack),
                [Action::Withdraw {
                    thread: "t-orch".into(),
                    msg_ref: 7
                }]
            );
        }

        /// The decisions overlay: a tap selects a row, a tap on the selected one opens it; its
        /// filters move on with a tap; its ✕ closes it.
        #[test]
        fn the_decisions_overlay_works_with_taps() {
            let id = |st: &AppState, i: usize| st.decision_rows()[i].id.clone();
            let mut st = with_decisions();
            open_decisions(&mut st);
            frame(&mut st, 160, 30);
            let row = Target::Decision(id(&st, 1));
            tap(&mut st, 160, 30, row);
            assert_eq!(st.decision_selected(), 1);
            tap(&mut st, 160, 30, Target::SiteFilter);
            assert!(st.view.decision_filter.site.is_some());
            tap(&mut st, 160, 30, Target::ProviderFilter);
            assert!(st.view.decision_filter.provider.is_some());
            st.view.decision_filter = DecisionFilter::default();
            let row = Target::Decision(id(&st, 1));
            tap(&mut st, 160, 30, row);
            assert_eq!(st.view.screen, Screen::Decisions, "selected");
            let row = Target::Decision(id(&st, 1));
            tap(&mut st, 160, 30, row);
            assert_ne!(st.view.screen, Screen::Decisions, "opened");
            open_decisions(&mut st);
            tap(&mut st, 160, 30, Target::Close);
            assert_ne!(st.view.screen, Screen::Decisions);
        }

        /// The orchestrator picker: a tap picks a runtime, a tap on the picked one chooses it,
        /// the wheel moves the pick; its ✕ decides later.
        #[test]
        fn the_picker_works_with_taps() {
            let mut st = three_threads();
            st.view.picker = Picker::from_show(&first_show());
            frame(&mut st, 100, 30);
            tap(&mut st, 100, 30, Target::Runtime(0));
            assert_eq!(st.view.picker.as_ref().unwrap().selected, 0);
            let inner = st.view.panes.get(&PaneId::Text).unwrap().area;
            wheel_at(&mut st, 100, 30, inner.x + 1, inner.y + 1, false);
            assert_eq!(st.view.picker.as_ref().unwrap().selected, 1);
            tap(&mut st, 100, 30, Target::Runtime(0));
            assert_eq!(
                tap(&mut st, 100, 30, Target::Runtime(0)),
                [Action::UseOrchestrator {
                    runtime: "claude-code".into(),
                    model: None,
                    summary: false,
                }]
            );
            st.view.picker = Picker::from_show(&first_show());
            tap(&mut st, 100, 30, Target::Close);
            assert!(st.view.picker.is_none());
        }
        /// An arm is for the item it was made on while it stays on screen. An
        /// option armed on gamma's question, then a permission on top of it, then the question
        /// back unchanged: the arm is gone, a tap only arms again.
        #[test]
        fn an_arm_goes_when_its_item_leaves_the_popup() {
            let mut st = three_threads();
            let mut c = ThreadScript::new("t-gamma");
            c.at(50);
            let mut q = c.raise("q1", Question);
            if let EventBody::AttentionRaised(p) = &mut q.body {
                p.summary = "which branch?".into();
                p.options = vec!["main".into(), "dev".into()];
            }
            control(&mut st, &[q]);
            tap_pane(&mut st, 160, 40, PaneId::Thread("t-gamma".into()));
            st.view.last_click = None;
            tap_pane(&mut st, 160, 40, PaneId::Thread("t-gamma".into()));
            assert!(st.action_popup().is_some());
            read(&mut st);
            tap(&mut st, 160, 40, Target::Row(ActionKind::Answer(1)));
            assert!(st.view.action.as_ref().unwrap().armed.is_some());
            // A permission comes on top, then goes.
            let p = c.raise("pg", Permission);
            control(&mut st, &[p]);
            frame(&mut st, 160, 40);
            let gone = c.event(EventBody::AttentionResolved(AttentionResolved {
                attn_id: "pg".into(),
                by: ResolvedBy::Timeout,
                ..Default::default()
            }));
            control(&mut st, &[gone]);
            frame(&mut st, 160, 40);
            read(&mut st);
            let out = tap(&mut st, 160, 40, Target::Row(ActionKind::Answer(1)));
            assert!(!responds(&out), "{out:?}");
        }
        /// An armed Stop does not run once its item is not on screen in full
        /// any more (a smaller screen left the popup no room for it).
        #[test]
        fn an_armed_stop_needs_its_item_still_on_screen() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            type_line(&mut st, "/n");
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-beta".into()));
            read(&mut st);
            tap(&mut st, 40, 20, Target::Row(ActionKind::Stop));
            assert!(st.view.action.as_ref().unwrap().armed.is_some());
            frame(&mut st, 40, 13);
            assert!(st.view.shown.is_none(), "no room for the item");
            let out = tap(&mut st, 40, 13, Target::Row(ActionKind::Stop));
            assert!(out.is_empty(), "{out:?}");
        }

        /// A decision that arrives between the press and the release on the
        /// selected row opens nothing: the row there is another decision now.
        #[test]
        fn a_decision_arriving_during_a_tap_opens_nothing() {
            let mut st = with_decisions();
            open_decisions(&mut st);
            let screen = frame(&mut st, 160, 30);
            let (r, _) = st
                .view
                .panes
                .targets
                .iter()
                .find(|(_, t)| matches!(t, Target::Decision(_)))
                .cloned()
                .unwrap();
            let left = MouseButton::Left;
            st.handle_mouse(mouse(MouseEventKind::Down(left), r.x + 2, r.y), &screen);
            // A worker's: opening it would dive into alpha.
            let mut a = ThreadScript::new("t-alpha");
            a.at(9000);
            let ev = decision(
                &mut a,
                Some(3),
                DecisionSite::Grade,
                DecisionProvider::Code,
                &["success", "partial"],
                "success",
                1.0,
                0,
            );
            control(&mut st, &[ev]);
            let screen = frame(&mut st, 160, 30);
            st.handle_mouse(mouse(MouseEventKind::Up(left), r.x + 2, r.y), &screen);
            assert_eq!(st.view.screen, Screen::Decisions, "nothing opened");
        }

        /// On a phone (40 x 20) the picker's runtimes are tapped though its
        /// heading wraps, the first-run one and `/orch`'s.
        #[test]
        fn the_picker_runtimes_are_tappable_at_40x20() {
            // The first row of runtime `i`'s control (it may wrap over more).
            let first_row = |st: &AppState, i: usize| {
                st.view
                    .panes
                    .targets
                    .iter()
                    .find(|(_, t)| *t == Target::Runtime(i))
                    .map(|(r, _)| r.y)
                    .unwrap_or_else(|| panic!("no runtime {i}"))
            };
            let mut st = three_threads();
            st.view.picker = Picker::from_show(&first_show());
            let buf = frame(&mut st, 40, 20);
            for i in 0..3 {
                let y = first_row(&st, i);
                let row: String = (0..40).map(|x| buf[(x, y)].symbol()).collect();
                let name = &st.view.picker.as_ref().unwrap().runtimes[i].0;
                assert!(row.contains(name.as_str()), "{i}: {row}");
            }
            tap(&mut st, 40, 20, Target::Runtime(2));
            assert_eq!(st.view.picker.as_ref().unwrap().selected, 2);
            let mut st = with_orch();
            type_line(&mut st, "/orch");
            let buf = frame(&mut st, 40, 20);
            let n = st.view.picker.as_ref().unwrap().runtimes.len();
            for i in 0..n {
                let y = first_row(&st, i);
                let row: String = (0..40).map(|x| buf[(x, y)].symbol()).collect();
                let name = &st.view.picker.as_ref().unwrap().runtimes[i].0;
                assert!(row.contains(name.as_str()), "{i}: {row}");
            }
        }

        /// With the sidebar hidden, a long request read in its detail view
        /// opens its popup with a click.
        #[test]
        fn a_long_request_opens_its_popup_with_the_sidebar_hidden() {
            let mut st = with_orch();
            let mut d = ThreadScript::new("t-delta");
            d.at(70);
            let mut q = d.raise("q9", Permission);
            if let EventBody::AttentionRaised(p) = &mut q.body {
                p.summary = (0..60)
                    .map(|i| format!("step {i}: a long command line"))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            control(&mut st, &[d.created("delta"), d.turn_started("run"), q]);
            st.handle_key(ctrl('b'));
            frame(&mut st, 120, 30);
            st.select_request("q9");
            frame(&mut st, 120, 30);
            let card = st.view.panes.card.clone().expect("the detail view");
            tap_at(&mut st, 120, 30, card.area.x + 2, card.area.y + 1);
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Request("q9".into()))
            );
        }

        /// A note wrapped over two rows of the status bar is tappable on both.
        #[test]
        fn wrapped_status_notes_keep_their_controls() {
            let mut st = three_threads();
            st.handle_key(ctrl('b'));
            st.view.status = None;
            let buf = frame(&mut st, 40, 20);
            let rows: Vec<u16> = st
                .view
                .panes
                .targets
                .iter()
                .filter(|(_, t)| *t == Target::Sidebar)
                .map(|(r, _)| r.y)
                .collect();
            let status = st.view.panes.targets.len();
            assert!(!rows.is_empty(), "{status} {}", screen_text(&buf));
            tap(&mut st, 40, 20, Target::Sidebar);
            assert!(!st.view.sidebar_hidden);
        }

        /// A press that began on one revision of the item and is released on
        /// another arms nothing: the gesture was not about what is there now.
        #[test]
        fn a_press_across_a_revision_change_arms_nothing() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            type_line(&mut st, "/n");
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-beta".into()));
            read(&mut st);
            let screen = frame(&mut st, 40, 20);
            let (x, y) = spot(&st, &Target::Row(ActionKind::Answer(1)));
            let left = MouseButton::Left;
            st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
            let mut b = ThreadScript::new("t-beta");
            b.at(40);
            let mut again = b.raise("pm1", Permission);
            if let EventBody::AttentionRaised(p) = &mut again.body {
                p.summary = "Bash: rm -rf src".into();
            }
            control(&mut st, &[again]);
            frame(&mut st, 40, 20);
            read(&mut st);
            let screen = frame(&mut st, 40, 20);
            assert_eq!(spot(&st, &Target::Row(ActionKind::Answer(1))), (x, y));
            st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen);
            assert_eq!(st.view.action.as_ref().unwrap().armed, None);
        }

        /// Several notes on one row each keep their control.
        #[test]
        fn notes_side_by_side_each_keep_their_control() {
            let mut st = three_threads();
            st.handle_key(ctrl('b'));
            st.handle_key(ctrl('f'));
            st.view.status = None;
            frame(&mut st, 160, 30);
            assert!(drawn_control(&st, &Target::Sidebar));
            assert!(drawn_control(&st, &Target::FocusOff));
            tap(&mut st, 160, 30, Target::Sidebar);
            assert!(!st.view.sidebar_hidden);
            assert!(st.view.focus_mode);
        }

        /// A mention that moves in the popup during a tap inserts nothing:
        /// the row there names another thread now.
        #[test]
        fn a_mention_that_moves_during_a_tap_inserts_nothing() {
            let mut st = three_threads();
            typed(&mut st, "@");
            let screen = frame(&mut st, 120, 30);
            let pop = st.view.panes.get(&PaneId::Popup).unwrap().area;
            let first: String = (pop.x..pop.right())
                .map(|x| screen[(x, pop.y)].symbol())
                .collect();
            let left = MouseButton::Left;
            st.handle_mouse(mouse(MouseEventKind::Down(left), pop.x + 1, pop.y), &screen);
            // The thread in the first row is parked: it goes after the live ones.
            let id = ["t-alpha", "t-beta", "t-gamma"]
                .into_iter()
                .find(|id| first.contains(&id[2..]))
                .expect("a thread in the first row");
            let mut t = ThreadScript::new(id);
            t.at(90);
            control(&mut st, &[t.parked()]);
            let screen = frame(&mut st, 120, 30);
            let now: String = (pop.x..pop.right())
                .map(|x| screen[(x, pop.y)].symbol())
                .collect();
            assert_ne!(first, now, "it moved");
            st.handle_mouse(mouse(MouseEventKind::Up(left), pop.x + 1, pop.y), &screen);
            assert_eq!(st.view.input.as_str(), "@");
        }

        /// A quick double click on the selected tile dives in though its
        /// popup, opened by the first click, now covers the spot.
        #[test]
        fn a_double_click_on_a_selected_tile_dives_under_its_popup() {
            let mut st = three_threads();
            st.handle_key(ctrl('g'));
            st.set_focus(Some("t-beta".into()));
            frame(&mut st, 80, 20);
            let tile = st
                .view
                .panes
                .get(&PaneId::Thread("t-beta".into()))
                .unwrap()
                .area;
            let (x, y) = (tile.x + 2, tile.bottom() - 1);
            st.view.last_click = None;
            tap_at(&mut st, 80, 20, x, y);
            assert!(st.action_popup().is_some(), "the first click opens it");
            assert_eq!(
                st.view.panes.at(x, y).map(|p| p.id.clone()),
                Some(PaneId::Popup),
                "it covers the spot"
            );
            tap_at(&mut st, 80, 20, x, y);
            assert_eq!(st.view.dive.as_deref(), Some("t-beta"));
        }

        /// A press on `yes` whose question was cancelled and replaced by
        /// another while the button was down answers nothing.
        #[test]
        fn a_yes_pressed_on_one_question_never_answers_another() {
            let mut st = three_threads();
            type_line(&mut st, "/park alpha");
            frame(&mut st, 120, 30);
            read(&mut st);
            let screen = frame(&mut st, 120, 30);
            let (x, y) = spot(&st, &Target::Yes);
            let left = MouseButton::Left;
            st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
            st.handle_key(key(KeyCode::Esc));
            type_line(&mut st, "/park gamma");
            frame(&mut st, 120, 30);
            read(&mut st);
            let screen = frame(&mut st, 120, 30);
            assert_eq!(spot(&st, &Target::Yes), (x, y));
            let out = st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen);
            assert!(out.is_empty(), "{out:?}");
        }

        /// The `/` popup that came up where a tile was just clicked is not
        /// that tile's popup: a click on it is no double click.
        #[test]
        fn a_slash_popup_over_a_clicked_tile_is_no_double_click() {
            let mut st = three_threads();
            st.handle_key(ctrl('g'));
            frame(&mut st, 80, 30);
            let tile = st
                .view
                .panes
                .get(&PaneId::Thread("t-gamma".into()))
                .unwrap()
                .area;
            let (x, y) = (tile.x + 2, tile.bottom() - 1);
            tap_at(&mut st, 80, 30, x, y);
            assert_eq!(st.focused(), Some("t-gamma"));
            typed(&mut st, "/");
            frame(&mut st, 80, 30);
            assert_eq!(
                st.view.panes.at(x, y).map(|p| p.id.clone()),
                Some(PaneId::Popup),
                "the / popup covers the spot"
            );
            tap_at(&mut st, 80, 30, x, y);
            assert_eq!(st.view.dive, None);
        }

        /// A row pressed in one card's popup and released after the popup
        /// moved to another card (Shift-↓), nothing waiting on either, runs nothing.
        #[test]
        fn a_row_pressed_in_one_cards_popup_never_runs_in_another() {
            let mut st = three_threads();
            let mut d = ThreadScript::new("t-delta");
            d.at(70);
            control(&mut st, &[d.created("delta")]);
            st.set_focus(Some("t-gamma".into()));
            frame(&mut st, 160, 40);
            st.handle_key(key(KeyCode::Enter));
            let screen = frame(&mut st, 160, 40);
            let (x, y) = spot(&st, &Target::Row(ActionKind::Talk));
            let left = MouseButton::Left;
            st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
            st.handle_key(shift(KeyCode::Down));
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Card("t-delta".into()))
            );
            let screen = frame(&mut st, 160, 40);
            assert_eq!(spot(&st, &Target::Row(ActionKind::Talk)), (x, y));
            st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen);
            assert_eq!(st.view.dive, None);
            assert!(st.action_popup().is_some(), "it stays");
        }

        /// A revision that moves another row under the finger leaves the
        /// popup open, saying so, rather than taking the release as a click elsewhere.
        #[test]
        fn a_revision_that_moves_the_rows_under_a_press_keeps_the_popup() {
            let mut st = compact();
            frame(&mut st, 40, 20);
            type_line(&mut st, "/n");
            tap_pane(&mut st, 40, 20, PaneId::Thread("t-beta".into()));
            let screen = frame(&mut st, 40, 20);
            let (x, y) = spot(&st, &Target::Row(ActionKind::Answer(1)));
            let left = MouseButton::Left;
            st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
            let mut b = ThreadScript::new("t-beta");
            b.at(40);
            let mut again = b.raise("pm1", Permission);
            if let EventBody::AttentionRaised(p) = &mut again.body {
                p.summary = "Bash: rm -rf target".into();
                p.options = vec!["allow".into(), "allow always".into(), "deny".into()];
            }
            control(&mut st, &[again]);
            let screen = frame(&mut st, 40, 20);
            assert_ne!(
                st.view.panes.target_at(x, y),
                Some(&Target::Row(ActionKind::Answer(1))),
                "another row there now"
            );
            let out = st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen);
            assert!(out.is_empty());
            assert!(st.action_popup().is_some(), "it stays");
        }

        /// In a dive with the sidebar hidden, an item read in its detail view
        /// opens its popup again with a click.
        #[test]
        fn a_detail_view_in_a_dive_opens_its_popup_with_a_click() {
            let mut st = three_threads();
            let mut d = ThreadScript::new("t-delta");
            d.at(70);
            let mut q = d.raise("q7", Review);
            if let EventBody::AttentionRaised(p) = &mut q.body {
                p.summary = (0..60)
                    .map(|i| format!("finding {i}: a long review line"))
                    .collect::<Vec<_>>()
                    .join("\n");
                p.options = vec!["retry".into(), "dismiss".into()];
            }
            control(&mut st, &[d.created("delta"), d.turn_started("review"), q]);
            st.handle_key(ctrl('b'));
            type_line(&mut st, "/open delta");
            st.handle_key(key(KeyCode::Enter));
            frame(&mut st, 120, 30);
            frame(&mut st, 120, 30);
            assert_eq!(st.detail_card(), Some("t-delta"));
            st.handle_key(key(KeyCode::Esc));
            frame(&mut st, 120, 30);
            assert!(st.view.action.is_none());
            let card = st.view.panes.card.clone().expect("the detail view");
            tap_at(&mut st, 120, 30, card.area.x + 2, card.area.y + 1);
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Card("t-delta".into()))
            );
        }

        /// A grid with no tile still has its way back.
        #[test]
        fn an_empty_grid_keeps_its_way_back() {
            let mut st = AppState::new();
            st.handle_key(ctrl('g'));
            assert_eq!(st.view.screen, Screen::Grid);
            frame(&mut st, 120, 30);
            tap(&mut st, 120, 30, Target::Stack);
            assert_eq!(st.view.screen, Screen::Stack);
        }

        /// In Focus mode with every worker parked, the note on them lists them
        /// and leaves Focus mode on.
        #[test]
        fn the_parked_note_lists_them_in_focus_mode() {
            let mut st = AppState::new();
            let mut p = ThreadScript::new("t-park");
            p.at(10);
            control(&mut st, &[p.created("park"), p.parked()]);
            st.handle_key(ctrl('f'));
            let screen = screen_text(&frame(&mut st, 120, 30));
            assert!(screen.contains("/parked shows"), "{screen}");
            tap(&mut st, 120, 30, Target::Parked);
            assert!(st.view.show_parked);
            assert!(st.view.focus_mode);
        }

        /// A take-back press is about the message it was pressed for: another
        /// one sent while the button was down is not taken back by its release.
        #[test]
        fn a_take_back_press_never_takes_back_a_later_message() {
            let mut st = with_orch();
            send_line(
                &mut st,
                "deploy api",
                serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 7,
                                   "turn": null, "pending": 1}),
            );
            let screen = frame(&mut st, 120, 30);
            let (x, y) = spot(&st, &Target::TakeBack);
            let left = MouseButton::Left;
            st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), &screen);
            send_line(
                &mut st,
                "deploy web",
                serde_json::json!({"action": "delegate", "thread": "t-orch", "ref": 8,
                                   "turn": null, "pending": 1}),
            );
            st.handle_key(key(KeyCode::Esc));
            assert!(st.unsend_hint().is_some(), "Esc armed the second message");
            let screen = frame(&mut st, 120, 30);
            assert_eq!(spot(&st, &Target::TakeBack), (x, y));
            let out = st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen);
            assert!(out.is_empty(), "{out:?}");
        }
    }

    /// The queue of the main pane's thread ([`crate::queue`]).
    mod queued {
        use super::*;
        use crate::select::Target;
        use serde_json::json;

        /// The orchestrator conversation with `items` queued (`total` waiting in all), as the
        /// hub listed them.
        fn with_queue(items: Value, total: u64) -> AppState {
            let mut st = with_orch();
            assert_eq!(st.queue_due(Instant::now()).as_deref(), Some("t-orch"));
            st.apply_result(
                "thread.queue",
                &json!({"thread": "t-orch", "thread_name": "orchestrator", "total": total,
                        "held": false, "items": items}),
            );
            st
        }

        fn three() -> Value {
            json!([
                {"ref": 7, "prompt": "fix the tests too", "truncated": false,
                 "from": "user", "mine": true},
                {"ref": 8, "prompt": "check the api\nand the web", "truncated": false,
                 "from": "orchestrator", "mine": false},
                {"ref": 9, "prompt": "then deploy", "truncated": true,
                 "from": "user", "mine": true},
            ])
        }

        /// They are listed right above the input box, the next first: who sent one when it
        /// is not the user, `…` when there is more of it, `↑ edits` on the user's newest
        /// while the line is empty, and how many more wait.
        #[test]
        fn queued_messages_are_listed_above_the_input_box() {
            let mut st = with_queue(three(), 5);
            let buf = draw(&mut st, 120, 30);
            let rows: Vec<String> = screen_text(&buf).lines().map(str::to_string).collect();
            let at = |needle: &str| {
                rows.iter()
                    .position(|r| r.contains(needle))
                    .unwrap_or_else(|| panic!("no {needle:?} in\n{}", rows.join("\n")))
            };
            let first = at("⧗ queued · fix the tests too");
            assert_eq!(
                at("⧗ queued · (from the orchestrator) check the api …"),
                first + 1
            );
            assert_eq!(at("⧗ queued · then deploy …"), first + 2);
            assert!(rows[first + 2].contains("↑ edits"), "{}", rows[first + 2]);
            assert!(!rows[first].contains("↑ edits"));
            assert_eq!(at("+ 2 more queued"), first + 3);
            // The input box's top border right under them.
            assert!(
                rows[first + 4].contains("→ orchestrator"),
                "{}",
                rows[first + 4]
            );
            // Typed text: no `↑ edits`.
            typed(&mut st, "x");
            let screen = screen_text(&draw(&mut st, 120, 30));
            assert!(!screen.contains("↑ edits"), "{screen}");
            // Another thread on the main pane: not its queue.
            st.view.input.clear();
            st.dive("t-alpha");
            let screen = screen_text(&draw(&mut st, 120, 30));
            assert!(!screen.contains("⧗ queued"), "{screen}");
        }

        /// ↑ on an empty line takes the user's newest queued message back (never the
        /// orchestrator's); its whole text, as the hub returns it, goes in the box, and Enter
        /// sends it again as a new message. With text typed, ↑ edits that.
        #[test]
        fn up_on_an_empty_line_takes_the_newest_queued_message_back_to_edit() {
            let mut st = with_queue(three(), 3);
            draw(&mut st, 120, 30);
            typed(&mut st, "draft");
            assert!(st.handle_key(key(KeyCode::Up)).is_empty());
            st.view.input.clear();
            let out = st.handle_key(key(KeyCode::Up));
            assert_eq!(
                out,
                vec![Action::Withdraw {
                    thread: "t-orch".into(),
                    msg_ref: 9
                }]
            );
            // A second ↑ before the answer asks nothing again.
            assert!(st.handle_key(key(KeyCode::Up)).is_empty());
            let full = format!("then deploy {}", "x".repeat(3000));
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 9, "thread": "t-orch",
                        "prompt": full}),
            );
            assert_eq!(st.view.input.as_str(), full);
            assert!(
                st.view
                    .status
                    .as_deref()
                    .is_some_and(|s| s.contains("taken back from the queue")),
                "{:?}",
                st.view.status
            );
            let sent = st.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(&sent[..], [Action::Say { text, .. }] if *text == full),
                "{sent:?}"
            );
        }

        /// The queue is read when the main pane shows a thread, when something about it
        /// changed, often while the thread works or messages wait, now and then otherwise; not
        /// while a read is on its way.
        #[test]
        fn the_queue_is_read_when_it_changes_and_while_it_moves() {
            let now = Instant::now();
            let mut st = with_orch();
            assert_eq!(st.queue_due(now).as_deref(), Some("t-orch"));
            assert_eq!(st.queue_due(now), None, "a read is on its way");
            st.apply_result(
                "thread.queue",
                &json!({"thread": "t-orch", "total": 0, "items": []}),
            );
            let busy = st
                .registry
                .get("t-orch")
                .is_some_and(|v| v.current_turn.is_some());
            // Idle with nothing queued: still read now and then (another client may queue
            // for a thread that can't take it, which starts no turn).
            let idle = st.queue_deadline(now).unwrap();
            let poll = if busy {
                crate::queue::QUEUE_POLL
            } else {
                crate::queue::QUEUE_IDLE_POLL
            };
            assert!(idle > now && idle <= now + poll + Duration::from_secs(1));
            // A message that queued: read again at once.
            st.apply_result(
                "say",
                &json!({"action": "delegate", "thread": "t-orch", "ref": 12, "turn": null,
                        "pending": 1}),
            );
            assert_eq!(st.queue_deadline(now), Some(now));
            assert_eq!(st.queue_due(now).as_deref(), Some("t-orch"));
            st.apply_result(
                "thread.queue",
                &json!({"thread": "t-orch", "total": 1, "items": [
                    {"ref": 12, "prompt": "x", "from": "user", "mine": true}]}),
            );
            // Messages wait: read again after a while.
            let next = st.queue_deadline(now).unwrap();
            assert!(next > now && next <= now + crate::queue::QUEUE_POLL + Duration::from_secs(1));
            // A failed read (an older hub): the next one waits longer.
            assert_eq!(st.queue_due(next).as_deref(), Some("t-orch"));
            st.apply_result("thread.queue", &Value::Null);
            assert!(st.queue_deadline(next).unwrap() >= now + Duration::from_secs(29));
        }

        /// Shift-↓ from the conversation goes on into the queued rows, Shift-↑ back; Enter
        /// opens a selected one's popup: edit (the user's), delete, and send now (the next
        /// one only); Esc releases it.
        #[test]
        fn a_selected_queued_message_opens_a_popup_to_edit_delete_or_send_it_now() {
            let mut st = with_queue(three(), 3);
            draw(&mut st, 120, 30);
            st.handle_key(shift(KeyCode::Left));
            let mut seen = Vec::new();
            for _ in 0..12 {
                st.handle_key(shift(KeyCode::Down));
                seen.push(st.view.queued);
            }
            assert_eq!(seen.last(), Some(&Some(9)), "{seen:?}");
            assert!(
                seen.contains(&Some(7)) && seen.contains(&Some(8)),
                "{seen:?}"
            );
            st.handle_key(shift(KeyCode::Up));
            assert_eq!(st.view.queued, Some(8));
            // The orchestrator's: delete only (not the next one: no send now).
            assert_eq!(st.selection_target(), Some(ActionTarget::Queued(8)));
            let rows = st.action_rows(&ActionTarget::Queued(8));
            assert_eq!(
                rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
                vec![ActionKind::Delete]
            );
            st.handle_key(shift(KeyCode::Up));
            assert_eq!(st.view.queued, Some(7));
            let rows = st.action_rows(&ActionTarget::Queued(7));
            assert_eq!(
                rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
                vec![ActionKind::Edit, ActionKind::Delete, ActionKind::SendNow]
            );
            // The popup opens on edit, never on what Enter would not undo.
            draw(&mut st, 120, 30);
            assert!(st.handle_key(key(KeyCode::Enter)).is_empty());
            assert_eq!(st.view.action.as_ref().unwrap().sel, Some(ActionKind::Edit));
            let screen = screen_text(&draw(&mut st, 120, 30));
            assert!(screen.contains("QUEUED · yours"), "{screen}");
            st.handle_key(key(KeyCode::Esc));
            assert!(st.view.action.is_none());
            let out = run_row(&mut st, ActionKind::SendNow);
            assert_eq!(
                out,
                vec![Action::SendNow {
                    thread: "t-orch".into(),
                    msg_ref: 7
                }]
            );
            st.apply_result(
                "message.send_now",
                &json!({"sent_now": true, "state": "interrupting", "thread": "t-orch",
                        "thread_name": "orchestrator", "ref": 7, "turn": 3}),
            );
            assert!(
                st.view
                    .status
                    .as_deref()
                    .is_some_and(|s| s.contains("runs next")),
                "{:?}",
                st.view.status
            );
            // Delete: its text goes to the history.
            st.select_queued(9);
            draw(&mut st, 120, 30);
            let out = run_row(&mut st, ActionKind::Delete);
            assert_eq!(
                out,
                vec![Action::Withdraw {
                    thread: "t-orch".into(),
                    msg_ref: 9
                }]
            );
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 9,
                        "prompt": "then deploy, all of it"}),
            );
            assert_eq!(
                st.view.history.last().map(String::as_str),
                Some("then deploy, all of it")
            );
            assert!(st.view.input.is_empty());
            // Esc releases a selected row.
            st.select_queued(7);
            st.handle_key(key(KeyCode::Esc));
            assert_eq!(st.view.queued, None);
        }

        /// A message taken back comes back as the line that sends it again, never a command:
        /// one that starts with `!` or `/`, edited from the queue, deleted to the history, or
        /// taken back with Esc Esc after it was typed with its escape.
        #[test]
        fn a_message_taken_back_is_never_a_command_again() {
            let mut st = with_queue(
                json!([{"ref": 7, "prompt": "!rm -rf build", "from": "user", "mine": true}]),
                1,
            );
            draw(&mut st, 120, 30);
            st.handle_key(key(KeyCode::Up));
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 7,
                        "prompt": "!rm -rf build"}),
            );
            assert_eq!(st.view.input.as_str(), "\\!rm -rf build");
            let sent = st.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(&sent[..], [Action::Say { text, .. }] if text == "!rm -rf build"),
                "{sent:?}"
            );
            // Deleted: the history has the line that sends it.
            let mut st = with_queue(
                json!([{"ref": 8, "prompt": "/delete api", "from": "user", "mine": true}]),
                1,
            );
            draw(&mut st, 120, 30);
            st.select_queued(8);
            draw(&mut st, 120, 30);
            run_row(&mut st, ActionKind::Delete);
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 8, "prompt": "/delete api"}),
            );
            assert_eq!(
                st.view.history.last().map(String::as_str),
                Some("//delete api")
            );
            // Typed with its escape, taken back with Esc Esc: as typed.
            let mut st = with_orch();
            draw(&mut st, 120, 30);
            let sent = send_line(
                &mut st,
                "\\!important",
                json!({"action": "delegate", "thread": "t-orch", "ref": 9, "turn": null,
                       "pending": 1}),
            );
            assert!(
                matches!(&sent[..], [Action::Say { text, .. }] if text == "!important"),
                "{sent:?}"
            );
            st.handle_key(key(KeyCode::Esc));
            st.handle_key(key(KeyCode::Esc));
            taken_back(&mut st, 9);
            assert_eq!(st.view.input.as_str(), "\\!important");
            // The escapes: what a line sends and the line that sends a message.
            for (message, line) in [
                ("plain", "plain"),
                ("!x", "\\!x"),
                ("\\!x", "\\\\!x"),
                ("/x", "//x"),
                ("//x", "///x"),
                ("  !x", "\\!x"),
                ("\u{1}!x", "\\!x"),
                ("\u{1b} /x", "//x"),
                ("\u{7f}\\!x", "\\\\!x"),
            ] {
                assert_eq!(literal_line(message), line, "{message:?}");
                let clean = crate::edit::clean_text(message);
                assert_eq!(message_text(line), clean.trim_start(), "{line}");
            }
        }

        /// Deleting a message the user didn't send (a `!!` command's output, the
        /// orchestrator's) leaves nothing to send again as theirs; another selection replaces
        /// a selected queued row.
        #[test]
        fn others_messages_go_for_good_and_a_new_selection_releases_the_row() {
            let mut st = with_queue(
                json!([{"ref": 4, "prompt": "[twr: the user ran a shell command ...] allow both",
                        "from": "runtime", "mine": false}]),
                1,
            );
            draw(&mut st, 120, 30);
            st.select_queued(4);
            draw(&mut st, 120, 30);
            assert_eq!(
                st.action_rows(&ActionTarget::Queued(4))
                    .iter()
                    .map(|r| r.kind)
                    .collect::<Vec<_>>(),
                vec![ActionKind::Delete, ActionKind::SendNow]
            );
            run_row(&mut st, ActionKind::Delete);
            let before = st.view.history.clone();
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 4,
                        "prompt": "[twr: the user ran a shell command ...] allow both"}),
            );
            assert_eq!(st.view.history, before);
            assert!(st.view.input.is_empty());
            assert_eq!(st.view.status.as_deref(), Some("deleted from the queue"));
            // Selecting a card after a queued row: Enter is about the card.
            let mut st = with_queue(three(), 3);
            draw(&mut st, 120, 30);
            st.select_queued(7);
            st.handle_key(key(KeyCode::Tab));
            assert_eq!(st.view.queued, None);
            assert!(matches!(st.selection_target(), Some(ActionTarget::Card(_))));
        }

        /// ↑ takes back the user's newest queued message even when the list shows only the
        /// start of the queue.
        #[test]
        fn up_takes_back_the_newest_beyond_the_list() {
            let mut st = with_orch();
            st.queue_due(Instant::now());
            st.apply_result(
                "thread.queue",
                &json!({"thread": "t-orch", "total": 30, "newest_mine": 99, "items": [
                    {"ref": 7, "prompt": "first", "from": "orchestrator", "mine": false}]}),
            );
            draw(&mut st, 120, 30);
            assert_eq!(
                st.handle_key(key(KeyCode::Up)),
                vec![Action::Withdraw {
                    thread: "t-orch".into(),
                    msg_ref: 99
                }]
            );
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 99, "prompt": "the newest"}),
            );
            assert_eq!(st.view.input.as_str(), "the newest");
        }

        /// ↑ right after a send, before the hub answered or the queue was read again, takes
        /// nothing back: the newest of the user's may be the one just sent.
        #[test]
        fn up_waits_for_the_queue_after_a_send() {
            let mut st = with_queue(three(), 3);
            draw(&mut st, 120, 30);
            assert!(matches!(
                &type_line(&mut st, "and lint")[..],
                [Action::Say { .. }]
            ));
            assert!(st.handle_key(key(KeyCode::Up)).is_empty());
            assert!(st
                .view
                .status
                .as_deref()
                .unwrap()
                .contains("reading the queue"));
            st.apply_result(
                "say",
                &json!({"action": "delegate", "thread": "t-orch", "ref": 10, "turn": null,
                        "pending": 4}),
            );
            assert!(st.handle_key(key(KeyCode::Up)).is_empty(), "not read since");
            assert_eq!(st.queue_due(Instant::now()).as_deref(), Some("t-orch"));
            let mut items = three();
            items.as_array_mut().unwrap().push(json!(
                {"ref": 10, "prompt": "and lint", "from": "user", "mine": true}));
            st.apply_result(
                "thread.queue",
                &json!({"thread": "t-orch", "total": 4, "held": false, "items": items}),
            );
            assert_eq!(
                st.handle_key(key(KeyCode::Up)),
                vec![Action::Withdraw {
                    thread: "t-orch".into(),
                    msg_ref: 10
                }]
            );
        }

        /// In the compact conversation, Enter on a selected queued row opens its popup, not
        /// the oldest item waiting.
        #[test]
        fn compact_enter_opens_a_selected_queued_row() {
            let mut st = with_queue(three(), 3);
            st.view.compact_width = 200;
            draw(&mut st, 120, 30);
            assert!(st.compact());
            st.select_queued(7);
            draw(&mut st, 120, 30);
            st.handle_key(key(KeyCode::Enter));
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Queued(7))
            );
            // Another tab lists no queue: its selection goes, Enter is the list's.
            st.view.action = None;
            st.select_queued(7);
            st.show_pane(CompactPane::Threads);
            assert_eq!(st.view.queued, None);
            assert!(st.shown_queue().is_none());
            draw(&mut st, 120, 30);
            st.handle_key(key(KeyCode::Enter));
            assert_ne!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Queued(7))
            );
        }

        /// A tap selects a queued row, a tap on the selected one opens its popup.
        #[test]
        fn a_tap_selects_a_queued_message_and_another_opens_its_popup() {
            let mut st = with_queue(three(), 3);
            let screen = crate::ui::draw_offscreen(&mut st, 120, 30, Instant::now());
            let (r, _) = st
                .view
                .panes
                .targets
                .iter()
                .find(|(_, t)| *t == Target::Queued(8))
                .cloned()
                .expect("a queued row drawn");
            let (x, y) = (r.x + 2, r.y);
            let left = MouseButton::Left;
            let tap = |st: &mut AppState, screen: &Buffer| {
                st.handle_mouse(mouse(MouseEventKind::Down(left), x, y), screen);
                let screen = crate::ui::draw_offscreen(st, 120, 30, Instant::now());
                st.handle_mouse(mouse(MouseEventKind::Up(left), x, y), &screen);
                crate::ui::draw_offscreen(st, 120, 30, Instant::now())
            };
            let screen = tap(&mut st, &screen);
            assert_eq!(st.view.queued, Some(8));
            assert!(st.view.action.is_none());
            tap(&mut st, &screen);
            assert_eq!(
                st.action_popup().map(|p| p.target.clone()),
                Some(ActionTarget::Queued(8))
            );
        }
    }

    /// `!` commands ([`crate::shell`]).
    mod shell_cmds {
        use super::*;
        use serde_json::json;
        use twr_core::event::TaskUpdated;

        /// A `!` command's job events on `o`'s thread: its start, and its end when `end`.
        /// A job's update, as the hub records it.
        fn job_event(o: &mut ThreadScript, job: Value, running: bool) -> Event {
            let mut ev = o.event(EventBody::TaskUpdated(TaskUpdated {
                task_id: format!("job:{}", job["id"].as_str().unwrap()),
                running,
                description: String::new(),
                ext: json!({ "twr": { "job": job } }).as_object().cloned(),
                ..Default::default()
            }));
            ev.source = twr_core::event::Source::Hub;
            ev
        }

        fn start(id: &str, command: &str, share: bool) -> Value {
            json!({"id": id, "name": command, "argv": ["/bin/sh", "-c", command],
                   "wake": share, "by": "user:tui",
                   "shell": {"command": command, "cwd": "/work/api", "share": share}})
        }

        fn end(id: &str, command: &str, exit: i64, printed: &str) -> Value {
            json!({"id": id, "name": command, "outcome": "exited", "exit": exit,
                   "duration_ms": 1234, "printed": printed,
                   "shell": {"command": command, "cwd": "/work/api", "share": false}})
        }

        /// Enter on a `!` line runs it (`!!` shares it), in the dived thread or the
        /// orchestrator's folder; `\!` sends a message that starts with `!`; a bare `!` or a
        /// dive's `!cd` says what to do; the orchestrator view's `!cd` moves its folder.
        #[test]
        fn a_bang_line_runs_a_command() {
            let mut st = with_orch();
            draw(&mut st, 120, 30);
            assert_eq!(
                type_line(&mut st, "!ls -la"),
                vec![Action::ShellRun {
                    thread: None,
                    command: "ls -la".into(),
                    cwd: None,
                    share: false,
                    launch: 1
                }]
            );
            assert!(st.view.input.is_empty());
            assert_eq!(st.view.history.last().map(String::as_str), Some("!ls -la"));
            assert_eq!(
                type_line(&mut st, "!! cargo test"),
                vec![Action::ShellRun {
                    thread: None,
                    command: "cargo test".into(),
                    cwd: None,
                    share: true,
                    launch: 2
                }]
            );
            let sent = type_line(&mut st, "\\!important: read this");
            assert!(
                matches!(&sent[..], [Action::Say { text, .. }] if text == "!important: read this"),
                "{sent:?}"
            );
            assert!(type_line(&mut st, "!").is_empty());
            assert!(st
                .view
                .status
                .as_deref()
                .unwrap()
                .contains("type a command"));
            assert_eq!(st.view.input.as_str(), "!", "the line stays to fix");
            st.view.input.clear();
            assert_eq!(
                type_line(&mut st, "!cd src"),
                vec![Action::ShellCd {
                    cwd: None,
                    dir: Some("src".into())
                }]
            );
            st.apply_result("shell.cd", &json!({"cwd": "/work/src"}));
            assert_eq!(st.shell_cwd.as_deref(), Some("/work/src"));
            assert!(st.view.status.as_deref().unwrap().contains("/work/src"));
            assert_eq!(
                type_line(&mut st, "!pwd"),
                vec![Action::ShellRun {
                    thread: None,
                    command: "pwd".into(),
                    cwd: Some("/work/src".into()),
                    share: false,
                    launch: 3
                }]
            );
            // A command typed while a `!cd` is on its way waits for it: it would run in the
            // folder before.
            assert_eq!(
                type_line(&mut st, "!cd other"),
                vec![Action::ShellCd {
                    cwd: Some("/work/src".into()),
                    dir: Some("other".into())
                }]
            );
            assert!(type_line(&mut st, "!rm file").is_empty());
            assert!(st
                .view
                .status
                .as_deref()
                .unwrap()
                .contains("still changing"));
            assert_eq!(
                st.view.input.as_str(),
                "!rm file",
                "kept to run once it moved"
            );
            st.view.input.clear();
            // It failed: commands run where they did.
            st.apply_result("shell.cd", &json!({"failed": true}));
            assert_eq!(
                type_line(&mut st, "!rm file"),
                vec![Action::ShellRun {
                    thread: None,
                    command: "rm file".into(),
                    cwd: Some("/work/src".into()),
                    share: false,
                    launch: 4
                }]
            );
            // `!cd` alone: back to the orchestrator's folder.
            assert_eq!(
                type_line(&mut st, "!cd"),
                vec![Action::ShellCd {
                    cwd: None,
                    dir: None
                }]
            );
            // In a dive: its thread, its worktree.
            st.dive("t-alpha");
            assert_eq!(
                type_line(&mut st, "!git status"),
                vec![Action::ShellRun {
                    thread: Some("t-alpha".into()),
                    command: "git status".into(),
                    cwd: None,
                    share: false,
                    launch: 5
                }]
            );
            assert!(type_line(&mut st, "!cd src").is_empty());
            assert!(st
                .view
                .status
                .as_deref()
                .unwrap()
                .contains("runs in its worktree"));
        }

        /// The TUI's ask for the folder's name and a user's `!cd` answer in the order sent:
        /// the name, coming in while the `!cd` is still on its way, neither sets the folder nor
        /// lets a command run before the `!cd` answered.
        #[test]
        fn a_late_folder_lookup_doesnt_undo_a_cd_on_its_way() {
            let mut st = with_orch();
            draw(&mut st, 120, 30);
            typed(&mut st, "!");
            assert!(st.shell_cwd_due(), "the folder's name is asked");
            st.view.input.clear();
            assert_eq!(
                type_line(&mut st, "!cd /work/x"),
                vec![Action::ShellCd {
                    cwd: None,
                    dir: Some("/work/x".into())
                }]
            );
            st.apply_result("shell.cd", &json!({"cwd": "/home/me"}));
            assert_eq!(st.shell_cwd, None);
            assert!(type_line(&mut st, "!rm file").is_empty(), "still waiting");
            st.view.input.clear();
            st.apply_result("shell.cd", &json!({"cwd": "/work/x"}));
            assert_eq!(st.shell_cwd.as_deref(), Some("/work/x"));
            assert_eq!(
                type_line(&mut st, "!rm file"),
                vec![Action::ShellRun {
                    thread: None,
                    command: "rm file".into(),
                    cwd: Some("/work/x".into()),
                    share: false,
                    launch: 1
                }]
            );
        }

        /// Ctrl-C right after Enter, before the hub answered: the command stops as soon as its
        /// job is known, and the next Ctrl-C quits.
        #[test]
        fn ctrl_c_before_the_answer_stops_the_command_once_it_starts() {
            let mut st = with_orch();
            draw(&mut st, 120, 30);
            assert_eq!(type_line(&mut st, "!sleep 99").len(), 1);
            assert_eq!(
                st.handle_key(ctrl('c')),
                vec![Action::ShellStopLaunch { launch: 1 }]
            );
            assert!(st.view.status.as_deref().unwrap().contains("as it starts"));
            st.apply_result(
                "shell.run",
                &json!({"job": "J9", "thread": "t-orch", "name": "sleep 99", "cwd": "/w"}),
            );
            assert!(st.shell_jobs.iter().any(|s| s.job == "J9" && s.stopping));
            assert_eq!(st.handle_key(ctrl('c')), vec![Action::Quit]);
        }

        /// A switch to another workspace and back keeps where `!` commands run, the commands
        /// Ctrl-C stops, and an answer that came meanwhile.
        #[test]
        fn a_workspace_switch_keeps_the_commands_state() {
            let mut st = with_orch();
            draw(&mut st, 120, 30);
            type_line(&mut st, "!cd /w/x");
            st.apply_result("shell.cd", &json!({"cwd": "/w/x"}));
            type_line(&mut st, "!sleep 99");
            let kept = st.take_view();
            let mut back = with_orch();
            back.restore_view(kept);
            back.apply_result(
                "shell.run",
                &json!({"job": "J5", "thread": "t-orch", "name": "sleep 99", "cwd": "/w/x"}),
            );
            back.finish_restore();
            assert_eq!(back.shell_cwd.as_deref(), Some("/w/x"));
            assert_eq!(
                back.handle_key(ctrl('c')),
                vec![Action::JobCancel { job: "J5".into() }]
            );
        }

        /// A paste is never a key: a pasted `!` line runs only with a real Enter.
        #[test]
        fn a_pasted_bang_line_waits_for_enter() {
            let mut st = with_orch();
            draw(&mut st, 120, 30);
            st.handle_paste("!rm -rf build\n");
            assert_eq!(st.view.input.as_str(), "!rm -rf build\n");
            let out = st.handle_key(key(KeyCode::Enter));
            assert_eq!(
                out,
                vec![Action::ShellRun {
                    thread: None,
                    command: "rm -rf build".into(),
                    cwd: None,
                    share: false,
                    launch: 1
                }]
            );
        }

        /// The input box says when its line is a shell command, where it runs, and whether
        /// its output goes on.
        #[test]
        fn the_input_box_shows_shell_mode() {
            let mut st = with_orch();
            typed(&mut st, "!ls");
            let screen = screen_text(&draw(&mut st, 160, 30));
            assert!(
                screen.contains(
                    " ! shell  in the orchestrator's folder · Enter runs it · shown only"
                ),
                "{screen}"
            );
            st.view.input.clear();
            typed(&mut st, "!!ls");
            let screen = screen_text(&draw(&mut st, 160, 30));
            assert!(
                screen.contains(
                    " !! shell  in the orchestrator's folder · Enter runs it, then sends its \
                     output to the orchestrator"
                ),
                "{screen}"
            );
            // An `@` in a command is the shell's: no thread popup.
            st.view.input.clear();
            typed(&mut st, "!echo @al");
            assert!(st.popup().is_none());
        }

        /// A command shows in the conversation: running, with `■ stop`; then how it ended and
        /// the end of its output, all of it with Ctrl-O.
        #[test]
        fn a_command_shows_its_end_and_output() {
            let mut st = with_orch();
            let mut o = ThreadScript::new("t-orch");
            o.at(100);
            st.apply_event(&thread_item(&job_event(
                &mut o,
                start("J1", "cargo test", false),
                true,
            )));
            let screen = screen_text(&draw(&mut st, 120, 30));
            assert!(screen.contains("! cargo test"), "{screen}");
            assert!(
                screen.contains("⏵ running · Ctrl-C stops it · in /work/api · shown only · ■ stop"),
                "{screen}"
            );
            let printed: String = (1..=10).map(|i| format!("line {i}\n")).collect();
            st.apply_event(&thread_item(&job_event(
                &mut o,
                end("J1", "cargo test", 0, printed.trim_end()),
                false,
            )));
            let screen = screen_text(&draw(&mut st, 120, 30));
            assert!(
                screen.contains("✓ exit 0 · 1.2s · in /work/api · shown only"),
                "{screen}"
            );
            assert!(
                screen.contains("… 4 earlier lines (ctrl+o to expand)"),
                "{screen}"
            );
            assert!(
                screen.contains("line 10") && !screen.contains("line 4\n"),
                "{screen}"
            );
            st.handle_key(ctrl('o'));
            let screen = screen_text(&draw(&mut st, 120, 40));
            assert!(
                screen.contains("line 1 ") || screen.contains("line 1\n"),
                "{screen}"
            );
            assert!(!screen.contains("earlier line"), "{screen}");
        }

        /// Ctrl-C stops the newest command this TUI started on the main pane's thread, and
        /// quits only once none runs; a click on `■ stop` stops one too.
        #[test]
        fn ctrl_c_stops_a_command_before_it_quits() {
            let mut st = with_orch();
            assert_eq!(st.handle_key(ctrl('c')), vec![Action::Quit]);
            let mut o = ThreadScript::new("t-orch");
            o.at(100);
            // A command that ended before `shell.run` answered: nothing to stop.
            let quick = job_event(&mut o, end("J1", "true", 0, ""), false);
            st.apply_control(&quick);
            st.apply_result(
                "shell.run",
                &json!({"job": "J1", "thread": "t-orch", "name": "true", "cwd": "/work"}),
            );
            assert!(st.shell_jobs.is_empty());
            assert_eq!(st.handle_key(ctrl('c')), vec![Action::Quit]);
            st.apply_event(&thread_item(&job_event(
                &mut o,
                start("J2", "sleep 30", false),
                true,
            )));
            st.apply_result(
                "shell.run",
                &json!({"job": "J2", "thread": "t-orch", "name": "sleep 30", "cwd": "/work"}),
            );
            assert_eq!(
                st.handle_key(ctrl('c')),
                vec![Action::JobCancel { job: "J2".into() }]
            );
            assert!(st
                .view
                .status
                .as_deref()
                .unwrap()
                .contains("Ctrl-C again quits"));
            assert_eq!(st.handle_key(ctrl('c')), vec![Action::Quit]);
            // Its end (on both streams): nothing left to stop.
            let ended = job_event(&mut o, end("J2", "sleep 30", 0, ""), false);
            st.apply_control(&ended);
            st.apply_event(&thread_item(&ended));
            assert!(st.shell_jobs.is_empty());
            assert_eq!(st.view.status.as_deref(), Some("stopped `sleep 30`"));
            // The `■ stop` of a running one.
            st.apply_event(&thread_item(&job_event(
                &mut o,
                start("J3", "sleep 9", false),
                true,
            )));
            let screen = draw(&mut st, 120, 30);
            let text = screen_text(&screen);
            let y = text
                .lines()
                .position(|l| l.contains("■ stop"))
                .expect("a stop row") as u16;
            let left = MouseButton::Left;
            st.handle_mouse(mouse(MouseEventKind::Down(left), 10, y), &screen);
            let screen = draw(&mut st, 120, 30);
            let out = st.handle_mouse(mouse(MouseEventKind::Up(left), 10, y), &screen);
            assert_eq!(out, vec![Action::JobCancel { job: "J3".into() }]);
            // Pressed on it, released once a redraw put another command's `■ stop` there
            // (output streaming in moved the rows): nothing stops.
            st.shell_jobs.clear();
            let screen = draw(&mut st, 120, 30);
            st.handle_mouse(mouse(MouseEventKind::Down(left), 10, y), &screen);
            let screen = draw(&mut st, 120, 30);
            let pane = st
                .view
                .panes
                .panes
                .iter_mut()
                .find(|p| p.id == PaneId::Transcript)
                .unwrap();
            let row = usize::from(y - pane.area.y);
            pane.links[row] = Some(format!("{}J9", crate::ui::SHELL_STOP));
            let out = st.handle_mouse(mouse(MouseEventKind::Up(left), 10, y), &screen);
            assert!(
                !out.iter().any(|a| matches!(a, Action::JobCancel { .. })),
                "{out:?}"
            );
        }

        /// A running command's output is read every so often, and once more after it ended;
        /// what the read returns is shown as plain text.
        #[test]
        fn a_running_commands_output_is_read() {
            let now = Instant::now();
            let mut st = with_orch();
            // Live: the replay is over.
            st.transcript.as_mut().unwrap().catch_up = None;
            let mut o = ThreadScript::new("t-orch");
            o.at(100);
            st.apply_event(&thread_item(&job_event(
                &mut o,
                start("J4", "make", false),
                true,
            )));
            assert_eq!(st.shell_reads_due(now), vec!["J4".to_string()]);
            assert!(st.shell_reads_due(now).is_empty(), "one read at a time");
            st.apply_result(
                "job.log",
                &json!({"job": "J4", "text": "\u{1b}[32mbuilding\u{1b}[0m\n50%\r100%"}),
            );
            let Some(Entry::Shell { output, .. }) = st.transcript.as_ref().unwrap().entries.last()
            else {
                panic!()
            };
            assert_eq!(output, "building\n100%");
            assert!(st.shell_reads_due(now).is_empty(), "read just now");
            assert!(st.shell_deadline(now).is_some());
            // A read asked while it ran, answered after its end: what the end recorded stays,
            // and another read follows.
            let later = Instant::now() + crate::shell::SHELL_POLL + Duration::from_millis(50);
            assert_eq!(st.shell_reads_due(later), vec!["J4".to_string()]);
            st.apply_event(&thread_item(&job_event(
                &mut o,
                end("J4", "make", 2, "x"),
                false,
            )));
            st.apply_result("job.log", &json!({"job": "J4", "text": "stale"}));
            let Some(Entry::Shell { output, .. }) = st.transcript.as_ref().unwrap().entries.last()
            else {
                panic!()
            };
            assert_eq!(output, "x");
            // It ended: one more read, then none.
            assert_eq!(st.shell_reads_due(later), vec!["J4".to_string()]);
            st.apply_result("job.log", &json!({"job": "J4", "text": "all of it"}));
            assert!(st
                .shell_reads_due(later + Duration::from_secs(5))
                .is_empty());
            assert_eq!(st.shell_deadline(later), None);
        }

        /// A stop the hub refused is no longer pending: the status says why, and Ctrl-C stops
        /// the command again.
        #[test]
        fn a_failed_stop_can_be_tried_again() {
            let mut st = with_orch();
            let mut o = ThreadScript::new("t-orch");
            o.at(100);
            st.apply_event(&thread_item(&job_event(
                &mut o,
                start("J7", "sleep 30", false),
                true,
            )));
            st.apply_result(
                "shell.run",
                &json!({"job": "J7", "thread": "t-orch", "name": "sleep 30", "cwd": "/work"}),
            );
            let stop = vec![Action::JobCancel { job: "J7".into() }];
            assert_eq!(st.handle_key(ctrl('c')), stop);
            st.apply_result(
                "job.cancel",
                &json!({"failed": true, "job": "J7", "error": "connection lost"}),
            );
            let status = st.view.status.clone().unwrap();
            assert!(
                status.contains("could not stop `sleep 30` (connection lost)"),
                "{status}"
            );
            assert_eq!(st.handle_key(ctrl('c')), stop);
        }

        /// Ctrl-C twice before the hub answered the command: the second says its stop is not
        /// sent yet, the third quits.
        #[test]
        fn quitting_waits_one_more_ctrl_c_for_an_unsent_stop() {
            let mut st = with_orch();
            draw(&mut st, 120, 30);
            assert_eq!(type_line(&mut st, "!sleep 99").len(), 1);
            assert_eq!(
                st.handle_key(ctrl('c')),
                vec![Action::ShellStopLaunch { launch: 1 }]
            );
            assert!(st.handle_key(ctrl('c')).is_empty());
            assert!(st
                .view
                .status
                .as_deref()
                .unwrap()
                .contains("still on its way"));
            assert_eq!(st.handle_key(ctrl('c')), vec![Action::Quit]);
        }

        /// A command's end in the replay shows what the end recorded: its log is not read.
        #[test]
        fn a_replayed_commands_log_is_not_read() {
            let now = Instant::now();
            let mut st = with_orch();
            assert!(st.transcript.as_ref().unwrap().catching_up());
            let mut o = ThreadScript::new("t-orch");
            o.at(100);
            let started = job_event(&mut o, start("J6", "make", false), true);
            let ended = job_event(&mut o, end("J6", "make", 0, "done"), false);
            st.apply_event(&thread_item(&started));
            st.apply_event(&thread_item(&ended));
            assert!(st.shell_reads_due(now).is_empty());
            let Some(Entry::Shell { output, .. }) = st.transcript.as_ref().unwrap().entries.last()
            else {
                panic!()
            };
            assert_eq!(output, "done");
        }
    }

    /// Next-message suggestions ([`crate::suggest`]).
    mod suggestions {
        use super::*;
        use serde_json::json;

        /// The orchestrator conversation after its turn 1 ended live.
        fn ended(on: bool) -> (AppState, ThreadScript) {
            let mut st = with_orch();
            st.view.suggest = on;
            st.view.suggest_model = "haiku".into();
            let mut o = ThreadScript::new("t-orch");
            o.at(200);
            let started = o.turn_started("fix it");
            let ended = o.turn_ended(TurnResult::Completed);
            control(&mut st, &[started, ended]);
            (st, o)
        }

        /// The hub's answer to the ask that is due, on screen long enough to be taken.
        fn answer(st: &mut AppState, text: &str) {
            st.take_suggest_due();
            st.apply_result(
                "suggest.next",
                &json!({"thread": "t-orch", "turn": 1, "model": "haiku", "suggestion": text,
                        "cached": false, "stats": {"calls": 1, "failures": 0, "cost_usd": 0.0001}}),
            );
            if let Some(s) = st.suggestion.as_mut() {
                s.shown = Some(Instant::now() - crate::suggest::SUGGEST_SETTLE);
            }
        }

        /// Off by default: nothing is asked. On: one ask after a turn ended live, with the
        /// model configured; none while something is typed.
        #[test]
        fn a_suggestion_is_asked_after_a_turn_only_when_on() {
            let (mut st, _) = ended(false);
            assert_eq!(st.take_suggest_due(), None);
            let (mut st, mut o) = ended(true);
            assert_eq!(
                st.take_suggest_due(),
                Some(crate::suggest::Ask {
                    thread: "t-orch".into(),
                    turn: 1,
                    model: "haiku".into()
                })
            );
            assert_eq!(st.take_suggest_due(), None, "asked once");
            typed(&mut st, "draft");
            let started = o.turn_started("more");
            let ended = o.turn_ended(TurnResult::Completed);
            control(&mut st, &[started, ended]);
            assert_eq!(st.take_suggest_due(), None, "typing: not asked");
        }

        /// The answer shows as ghost text in the empty box; Enter there sends nothing; Tab or →
        /// puts it in the box to send; typing types over it.
        #[test]
        fn the_ghost_is_taken_with_tab_or_right_and_never_sent_by_itself() {
            let (mut st, _) = ended(true);
            st.take_suggest_due();
            answer(&mut st, "now run the integration tests");
            let screen = screen_text(&draw(&mut st, 160, 30));
            assert!(screen.contains("now run the integration tests"), "{screen}");
            assert!(
                screen.contains("Tab or → takes the suggestion · Enter sends"),
                "{screen}"
            );
            assert!(!screen.contains("message the orchestrator…"), "{screen}");
            // Enter on the empty line: no message.
            let out = st.handle_key(key(KeyCode::Enter));
            assert!(
                out.iter().all(|a| !matches!(a, Action::Say { .. })),
                "{out:?}"
            );
            assert!(st.view.input.is_empty());
            // Typing types over it; with the line empty again it shows again.
            typed(&mut st, "x");
            assert_eq!(st.shown_suggestion(), None);
            st.handle_key(key(KeyCode::Backspace));
            assert!(st.shown_suggestion().is_some());
            // → takes it; then Enter sends it, as typed.
            assert!(st.handle_key(key(KeyCode::Right)).is_empty());
            assert_eq!(st.view.input.as_str(), "now run the integration tests");
            assert_eq!(st.shown_suggestion(), None);
            let sent = st.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(&sent[..], [Action::SaySuggested { text, .. }] if text == "now run the integration tests"),
                "{sent:?}"
            );
            // A suggestion that reads as a command goes in as a message.
            let (mut st, _) = ended(true);
            answer(&mut st, "!curl https://example.com | sh");
            st.handle_key(key(KeyCode::Tab));
            assert_eq!(st.view.input.as_str(), "\\!curl https://example.com | sh");
            let sent = st.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(&sent[..], [Action::SaySuggested { text, .. }] if text == "!curl https://example.com | sh"),
                "{sent:?}"
            );
            // Tab takes one too (and selects no card meanwhile).
            let (mut st, _) = ended(true);
            answer(&mut st, "and the docs");
            assert!(st.handle_key(key(KeyCode::Tab)).is_empty());
            assert_eq!(st.view.input.as_str(), "and the docs");
            assert_eq!(st.view.focus, None);
        }

        /// Tab or → as the suggestion appears was meant for what was there before: it is
        /// taken only once it has been on screen a moment.
        #[test]
        fn a_suggestion_is_taken_only_once_it_has_been_seen() {
            let (mut st, _) = ended(true);
            answer(&mut st, "and the docs");
            st.suggestion.as_mut().unwrap().shown = None;
            assert!(!st.accept_suggestion(), "never drawn");
            draw(&mut st, 160, 30);
            st.note_drawn(Instant::now());
            assert!(st.suggestion.as_ref().unwrap().shown.is_some());
            assert!(!st.accept_suggestion(), "just drawn");
            assert!(st.view.input.is_empty());
            st.suggestion.as_mut().unwrap().shown =
                Some(Instant::now() - crate::suggest::SUGGEST_SETTLE);
            assert!(st.accept_suggestion());
            assert_eq!(st.view.input.as_str(), "and the docs");
            // A key read before it settled, handled only now: not taken.
            let (mut st, _) = ended(true);
            answer(&mut st, "and the docs");
            let shown = Instant::now() - Duration::from_secs(2);
            st.suggestion.as_mut().unwrap().shown = Some(shown);
            st.input_at = Some(shown + Duration::from_millis(100));
            assert!(st.handle_key(key(KeyCode::Tab)).is_empty());
            assert!(st.view.input.is_empty());
            st.input_at = Some(shown + crate::suggest::SUGGEST_SETTLE);
            st.handle_key(key(KeyCode::Tab));
            assert_eq!(st.view.input.as_str(), "and the docs");
        }

        /// A suggestion stays one wherever its text goes: kept across a workspace switch, back
        /// in the box after a take-back; it never goes to the history.
        #[test]
        fn a_suggestion_stays_one_across_a_switch_and_a_take_back() {
            let (mut st, _) = ended(true);
            answer(&mut st, "set web to bypass");
            st.handle_key(key(KeyCode::Tab));
            let kept = st.take_view();
            let mut st = with_orch();
            st.restore_view(kept);
            st.finish_restore();
            assert_eq!(st.view.input.as_str(), "set web to bypass");
            let sent = st.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(&sent[..], [Action::SaySuggested { text, .. }] if text == "set web to bypass"),
                "{sent:?}"
            );
            assert!(!st.view.history.iter().any(|h| h == "set web to bypass"));
            st.apply_result(
                "say",
                &json!({"action": "delegate", "thread": "t-orch", "ref": 61, "turn": null,
                        "pending": 1}),
            );
            assert_eq!(
                st.unsend(),
                vec![Action::Withdraw {
                    thread: "t-orch".into(),
                    msg_ref: 61
                }]
            );
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 61,
                        "prompt": "set web to bypass"}),
            );
            assert_eq!(st.view.input.as_str(), "set web to bypass");
            let sent = st.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(&sent[..], [Action::SaySuggested { .. }]),
                "{sent:?}"
            );
        }

        /// A suggestion sent, then a message of the user's before the first was answered:
        /// each answer registers its own send, so the queued suggestion is still one when it
        /// is taken back to edit.
        #[test]
        fn overlapping_sends_keep_the_suggestions_ref() {
            let (mut st, _) = ended(true);
            answer(&mut st, "set web to bypass");
            st.handle_key(key(KeyCode::Tab));
            assert!(matches!(
                &st.handle_key(key(KeyCode::Enter))[..],
                [Action::SaySuggested { .. }]
            ));
            assert!(matches!(
                &type_line(&mut st, "and lint")[..],
                [Action::Say { .. }]
            ));
            for r in [71, 72] {
                st.apply_result(
                    "say",
                    &json!({"action": "delegate", "thread": "t-orch", "ref": r, "turn": null,
                            "pending": 2}),
                );
            }
            assert!(st.suggested_refs.contains(&("t-orch".to_string(), 71)));
            assert!(!st.suggested_refs.contains(&("t-orch".to_string(), 72)));
            assert_eq!(st.queue_due(Instant::now()).as_deref(), Some("t-orch"));
            st.apply_result(
                "thread.queue",
                &json!({"thread": "t-orch", "total": 2, "items": [
                    {"ref": 71, "prompt": "set web to bypass", "from": "user", "mine": true},
                    {"ref": 72, "prompt": "and lint", "from": "user", "mine": true}]}),
            );
            st.retract_queued(71, crate::app::Retract::Edit);
            assert!(st
                .withdrawals
                .iter()
                .any(|w| w.msg_ref == 71 && w.suggested));
        }

        /// A suggestion sent in a dive and taken back is still one: out of the dive, sent to
        /// the orchestrator as it is, it goes as a suggestion.
        #[test]
        fn a_suggestion_sent_in_a_dive_stays_one() {
            let mut st = with_orch();
            draw(&mut st, 120, 30);
            st.dive("t-alpha");
            st.view.input.set("set web to bypass");
            st.taken_suggestion = Some("set web to bypass".into());
            let sent = st.handle_key(key(KeyCode::Enter));
            assert!(matches!(&sent[..], [Action::Send { .. }]), "{sent:?}");
            st.apply_result(
                "thread.send",
                &json!({"thread": "t-alpha", "ref": 81, "turn": null, "pending": 1}),
            );
            assert_eq!(
                st.unsend(),
                vec![Action::Withdraw {
                    thread: "t-alpha".into(),
                    msg_ref: 81
                }]
            );
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 81,
                        "prompt": "set web to bypass"}),
            );
            assert_eq!(st.view.input.as_str(), "set web to bypass");
            st.leave_dive();
            let sent = st.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(&sent[..], [Action::SaySuggested { .. }]),
                "{sent:?}"
            );
        }

        /// The queue listed a suggestion before its send was answered, and an edit took it
        /// back then: the send's answer, which comes before the withdrawal's, still marks it.
        /// Suggestions stay marked however many come after, while they are queued.
        #[test]
        fn a_suggestion_edited_before_its_answer_stays_one() {
            let (mut st, _) = ended(true);
            answer(&mut st, "set web to bypass");
            st.handle_key(key(KeyCode::Tab));
            assert!(matches!(
                &st.handle_key(key(KeyCode::Enter))[..],
                [Action::SaySuggested { .. }]
            ));
            assert_eq!(st.queue_due(Instant::now()).as_deref(), Some("t-orch"));
            st.apply_result(
                "thread.queue",
                &json!({"thread": "t-orch", "total": 1, "items": [
                    {"ref": 91, "prompt": "set web to bypass", "from": "user", "mine": true}]}),
            );
            st.retract_queued(91, crate::app::Retract::Edit);
            st.apply_result(
                "say",
                &json!({"action": "delegate", "thread": "t-orch", "ref": 91, "turn": null,
                        "pending": 1}),
            );
            st.apply_result(
                "message.withdraw",
                &json!({"withdrawn": true, "state": "queued", "ref": 91,
                        "prompt": "set web to bypass"}),
            );
            assert_eq!(st.view.input.as_str(), "set web to bypass");
            assert_eq!(st.taken_suggestion.as_deref(), Some("set web to bypass"));
            // Many suggestions later, the first still queued is still one.
            st.view.input.clear();
            st.taken_suggestion = None;
            for i in 0..40 {
                let text = format!("suggestion {i}");
                st.view.input.set(&text);
                st.taken_suggestion = Some(text);
                st.handle_key(key(KeyCode::Enter));
                st.apply_result(
                    "say",
                    &json!({"action": "delegate", "thread": "t-orch", "ref": 100 + i,
                            "turn": null, "pending": 1}),
                );
            }
            assert!(st.suggested_refs.contains(&("t-orch".to_string(), 100)));
        }

        /// An answer to an ask made before a send of the user's is about a conversation that
        /// went on: it shows nothing.
        #[test]
        fn an_answer_from_before_a_send_shows_nothing() {
            let (mut st, _) = ended(true);
            st.take_suggest_due();
            assert!(matches!(
                &type_line(&mut st, "go on")[..],
                [Action::Say { .. }]
            ));
            st.apply_result(
                "suggest.next",
                &json!({"thread": "t-orch", "turn": 1, "suggestion": "and the docs"}),
            );
            assert!(st.suggestion.is_none());
        }

        /// A new turn takes the suggestion away, and an answer for a turn that is no longer the
        /// last shows nothing; a failed call says so once; /suggest turns them off.
        #[test]
        fn suggestions_go_with_the_next_turn_and_can_be_turned_off() {
            let (mut st, mut o) = ended(true);
            answer(&mut st, "next step");
            assert!(st.shown_suggestion().is_some());
            control(&mut st, &[o.turn_started("go on")]);
            assert_eq!(st.shown_suggestion(), None);
            // Late answer for turn 1 while turn 2 runs: not shown.
            answer(&mut st, "stale");
            assert_eq!(st.shown_suggestion(), None);
            st.apply_result(
                "suggest.next",
                &json!({"thread": "t-orch", "turn": 1, "suggestion": null,
                        "reason": "failed: Please run /login"}),
            );
            assert!(st
                .view
                .status
                .as_deref()
                .unwrap()
                .contains("Please run /login"));
            st.view.status = None;
            st.apply_result(
                "suggest.next",
                &json!({"thread": "t-orch", "turn": 1, "suggestion": null,
                        "reason": "failed: Please run /login"}),
            );
            assert_eq!(st.view.status, None, "said once");
            type_line(&mut st, "/suggest off");
            assert!(!st.view.suggest);
            assert_eq!(st.view.status.as_deref(), Some("suggestions off"));
            type_line(&mut st, "/suggest");
            assert!(st.view.suggest);
            assert!(st
                .view
                .status
                .as_deref()
                .unwrap()
                .contains("go to Anthropic"));
        }
    }
}
