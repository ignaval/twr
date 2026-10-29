//! Mouse text selection. Rendering records every text pane's inner area ([`Panes`]); a drag
//! that starts in a pane selects inside that pane only ([`Selection`]), so borders and other
//! panes are never part of it, and the text comes from the cells as drawn. Copying goes
//! through the terminal (OSC 52, [`osc52`]) and, outside SSH, a local clipboard command
//! ([`clipboard_command`]). Rendering also records what a click or a tap there does
//! ([`Target`]): the controls drawn to be tapped, a row each at least, over what they sit on.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;

use crate::edit::width;

/// What a pane shows; a click on a thread's pane focuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneId {
    /// A thread's card or grid tile ([`twr_core::event::HUB_THREAD`] for the hub's card), or
    /// the orchestrator's attention banner in the main pane (the orchestrator's id).
    Thread(String),
    /// The suggestion popup above the input line.
    Popup,
    /// The main pane: the orchestrator conversation, or the transcript of the thread dived
    /// into.
    Transcript,
    /// The input line (or the orchestrator line).
    Input,
    /// The decision table.
    Decisions,
    /// The help overlay.
    Help,
    /// A workspace's tab in the strip ([`crate::workspace`]): a click switches to it.
    Tab(String),
    /// The sidebar's workspace list or the compact layout's Threads screen
    /// ([`crate::sidebar`]): the wheel scrolls it.
    Workspaces,
    /// Any other text: the status bar, headers, the first-run picker.
    Text,
}

/// What a click (a tap, on a phone) on a control drawn to be tapped does, rather than on
/// text: rendering records each with its area ([`Panes::targets`]), drawn over what is under
/// it, and an overlay takes those under it away ([`Panes::cover`]). A drag never runs one: it
/// selects text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A compact screen's name on the line naming them: shows that screen (`/c`, `/n`, `/t`).
    Screen(crate::app::CompactPane),
    /// The dive's header (`‹ back`): back to the orchestrator conversation (`/back`).
    Back,
    /// The grid's header (`‹ stack`): back to the stack view (Ctrl-G).
    Stack,
    /// The main pane's bottom border while it is scrolled up (`↓ 12 lines below`): to the
    /// bottom, following it (End).
    Follow,
    /// The `✕` of what is open over the screen: the help, the trash, the action popup, the
    /// `/` or `@` popup, the orchestrator picker, the decisions overlay (Esc).
    Close,
    /// A detail view's `✕` (a request or an item read in the main pane): back to the
    /// conversation, as Esc releases it.
    Release,
    /// The input box's top border while the last message can be taken back: the first tap
    /// arms the second, which takes it back (Esc Esc).
    TakeBack,
    /// A row of the action popup: a tap highlights it, a tap on the highlighted row runs it
    /// (an answer, Dismiss or Stop only once a tap highlighted it on the item as it is now,
    /// read long enough).
    Row(crate::app::ActionKind),
    /// A row of the `/` or `@` popup, by the value it completes to (rows move as the
    /// threads change): completes it (Tab).
    Suggestion(String),
    /// A runtime of the orchestrator picker, by its index: picks it, and a tap on the picked
    /// one chooses it (Enter).
    Runtime(usize),
    /// A row of the decision table, by its decision's event id (rows move as decisions come
    /// in): selects it, and a tap on the selected one opens it (Enter).
    Decision(String),
    /// The decision table's site filter: the next one (`f`).
    SiteFilter,
    /// The decision table's provider filter: the next one (`v`).
    ProviderFilter,
    /// The question waiting for `y` in the status bar: `yes` runs it (once it has been on
    /// screen long enough to be read), `no` cancels it.
    Yes,
    /// See [`Target::Yes`].
    No,
    /// The notice in the status bar: dismisses it.
    Notice,
    /// The status bar's `Threads hidden · Ctrl-B shows`: shows the sidebar.
    Sidebar,
    /// The status bar's `Focus · N hidden · Ctrl-F all`: Focus mode off.
    FocusOff,
    /// A `N parked hidden · /parked shows them` note: lists the parked threads.
    Parked,
    /// A queued message listed above the input box, by its ref: selects it, and a tap on the
    /// selected one opens its action popup (Enter).
    Queued(i64),
    /// A workspace's card in the sidebar, its drawer or the compact layout's Threads screen
    /// (every row of the card, its threads' rows included): switches to it, as its tab does.
    Workspace(String),
    /// `↑ more` or `↓ more` where the sidebar or the compact Threads screen cut workspaces
    /// (the whole row): brings this one, the nearest cut, into view.
    WsMore(String),
    /// `+ New workspace` under the workspaces (the whole row): asks for the new one's name.
    NewWorkspace,
    /// `⚙ Settings` under the workspaces (the whole row): opens the settings, as `/settings`.
    Settings,
    /// The workspace sidebar's header (`‹`, the whole row) hides it, and the tab strip's `≡`
    /// shows it (or, where it does not fit, opens it as a drawer), as Ctrl-T does.
    WsSidebar,
    /// A row of the settings, by what it sets: selects it, and a tap on the selected one
    /// changes it (Enter).
    Setting(crate::settings::Key),
    /// On a phone, the row under a text cut to the rows it has, saying how much of it is
    /// below (`↓ 12 more lines`), or, once its end is in view, above: a tap scrolls it a page
    /// down, or back to its top ([`crate::app::AppState`]'s `page_text`). Which text: the
    /// action popup's ([`Panes::sheet`]) or the question waiting for `y`'s ([`Panes::ask`]).
    More(Scrolled),
}

/// A text a phone scrolls in the rows it has ([`Target::More`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scrolled {
    /// The action popup's text ([`Panes::sheet`]).
    Sheet,
    /// The question waiting for `y`'s prompt ([`Panes::ask`]).
    Ask,
}

/// How a drawn row continues the row above it in the copied text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Join {
    /// A row of its own: a line break before it.
    #[default]
    Newline,
    /// Wrapped at a space (the space was not drawn): one space before it.
    Space,
    /// A word split across rows: nothing before it.
    Glue,
    /// A row with no text, only drawing (a thread box's bottom border,
    /// [`crate::ui::Edge::Bottom`]): it adds nothing to a copy, not even a line break.
    Skip,
}

/// The transcript row at the top of the main pane while it is scrolled up: a place in one of
/// the transcript's entries or request cards, so the view stays on what is read there as
/// replies stream in below it, entries and cards are added, removed or folded around it, or
/// its rows wrap again at another width ([`crate::ui::anchor_row`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Anchor {
    /// The entry the place is in, by its id ([`crate::app::Transcript::ids`]); `None` in a
    /// request card drawn above the entries.
    pub entry: Option<u64>,
    /// The request whose card the place is in: found wherever the card is drawn, above the
    /// entries or in the entry of the update that raised it, while it is the same raise.
    /// Once the card is gone, the view is anchored on what is drawn instead.
    pub request: Option<RequestCard>,
    /// The section of the entry the place is in, from 0 ([`crate::ui::Spans::sections`]): a
    /// reply's thread boxes and the text around them are sections of its own, each a long
    /// block collapsed or expanded by itself.
    pub section: usize,
    /// The line of the section's text (or the card's) the place is on: a row of its own
    /// starts a line, a row wrapped from the one above continues it.
    pub line: usize,
    /// The characters (grapheme clusters) of that line before the place, but its whitespace
    /// and what is drawn around the text (a marker, an indentation, a box's frame,
    /// [`crate::ui::Edge`]): wrapping at another width neither adds nor drops any, so it is
    /// the row holding the same text at any width.
    pub chars: usize,
    /// The rows of that line before the place that start at the same character (rows of
    /// spaces in between): the row of a run of spaces the place is on.
    pub repeat: usize,
    /// Rows from there to the top of the view: moved by keys since it was drawn, or on a
    /// row of no entry or card (a blank row between them: rows above the next one).
    pub rows: isize,
}

/// A request's card as an anchor names it ([`Anchor::request`]): one raise of an attention id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestCard {
    /// The attention id.
    pub attn_id: String,
    /// The id of the event that raised it ([`twr_core::reduce::AttentionItem::event_id`]): it
    /// stays while the request is raised again before it is answered; answered and raised
    /// again under the same id, the request is another card, which an anchor never finds.
    pub raise: String,
}

/// A pane as drawn: its inner area (no border).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// What it shows.
    pub id: PaneId,
    /// Inner area.
    pub area: Rect,
    /// [`Join`] of each row from the top (rows past the end are [`Join::Newline`]); only
    /// panes whose wrapping the TUI does itself (the transcript) have them.
    pub joins: Vec<Join>,
    /// How far it can scroll up (the transcript's lines above its view), in rows.
    pub max_scroll: usize,
    /// The actual offset of this frame from the conversation's bottom, including request
    /// anchoring. Explicit scrolling starts here when it releases an inline selection.
    pub scroll: usize,
    /// Rows at its bottom drawn only while it follows the bottom (the working cursor's own,
    /// [`crate::ui`]'s `place_cursor`): scrolled up, they go.
    pub follow_rows: usize,
    /// The transcript's top row as drawn ([`Anchor`]); `None` with no entries.
    pub top: Option<Anchor>,
    /// The anchor the view keeps: the one it was drawn from, when it was drawn right there
    /// (so a view resized back and forth comes back where it was), else [`Pane::top`].
    pub anchor: Option<Anchor>,
    /// The long block selected ([`crate::app::ViewState::block`]) while the transcript drawn
    /// has it: folded away, or short enough at this width to be no block, it is not selected
    /// any more.
    pub block: Option<String>,
    /// What a click on each row from the top opens (the transcript's collapsed groups: the
    /// key of the group its marker row stands for); rows past the end open nothing.
    pub links: Vec<Option<String>>,
    /// Cells left out of copied text: the working icon ([`crate::icon`]) and the space after
    /// it, which change with every frame and are not the pane's text, and the frames of the
    /// transcript's thread boxes ([`crate::ui::Edge`]).
    pub skip: Vec<Rect>,
}

impl Pane {
    /// A pane `id` over `area`.
    pub fn new(id: PaneId, area: Rect) -> Self {
        Pane {
            id,
            area,
            joins: Vec::new(),
            max_scroll: 0,
            scroll: 0,
            follow_rows: 0,
            top: None,
            anchor: None,
            block: None,
            links: Vec::new(),
            skip: Vec::new(),
        }
    }

    /// What a click on row `y` (a screen row) opens, if anything ([`Pane::links`]).
    pub fn link(&self, y: u16) -> Option<&str> {
        self.links
            .get(usize::from(y.checked_sub(self.area.y)?))?
            .as_deref()
    }

    fn join(&self, y: u16) -> Join {
        self.joins
            .get(usize::from(y.saturating_sub(self.area.y)))
            .copied()
            .unwrap_or_default()
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        self.area.contains(Position { x, y })
    }
}

/// An attention item the last draw showed in full on a card, tile, banner or the action popup:
/// its badge and summary and every option it lists (the first nine with their keys, the rest
/// numbered `10.` on), each with its whole label, and its full text (never only the capped
/// preview of [`crate::app::Incomplete`]). Only such an item can be answered or dismissed from
/// the action popup ([`crate::app::AppState::note_drawn`]), and only such items count as seen
/// ([`crate::app::AppState::seen_gseq`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawnItem {
    /// The card's thread: a worker, [`twr_core::event::HUB_THREAD`] for the hub's card, or
    /// the orchestrator for its banner.
    pub thread: String,
    /// Attention id.
    pub attn_id: String,
    /// Its revision as drawn.
    pub revision: u64,
    /// The card's inner area.
    pub area: Rect,
}

/// Every pane of the last draw, in drawing order, and the attention items it showed in full.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Panes {
    /// The text panes, in drawing order.
    pub panes: Vec<Pane>,
    /// The status bar's message ([`crate::app::ViewState::status`]) and the bar's area, when
    /// the draw showed it in full, not cut and not under an overlay: a notice told once
    /// counts as told only then ([`crate::app::AppState::note_drawn`]).
    pub status: Option<(String, Rect)>,
    /// The attention items drawn in full ([`DrawnItem`]), none hidden under an overlay.
    pub items: Vec<DrawnItem>,
    /// The cells of the moving working icons drawn (their cursors), none hidden under an
    /// overlay.
    pub moving: Vec<Rect>,
    /// The draw showed a moving working icon ([`Panes::moving`], once the overlays are
    /// drawn): the IO loop draws the next frame on time ([`crate::icon::next_tick`]).
    pub animated: bool,
    /// The confirm card's detail view, when the draw showed it ([`CardWindow`]), not hidden
    /// under an overlay.
    pub card: Option<CardWindow>,
    /// On a phone, the action popup's text (its item, a report, a queued message) as drawn
    /// in the rows its options leave it ([`CardWindow`]): when it has more rows than that, the
    /// wheel over it, PgUp/PgDn and Ctrl-↑↓ scroll it, its options staying where they are
    /// ([`crate::app::ViewState::sheet_view`]).
    pub sheet: Option<CardWindow>,
    /// On a phone, the prompt of the question waiting for `y` as the status bar drew it
    /// ([`CardWindow`], its `attn_id` the prompt): when it has more rows than the status bar
    /// gives it, the wheel over it scrolls it ([`crate::app::ViewState::ask_view`]).
    pub ask: Option<CardWindow>,
    /// The trash overlay's furthest scroll as drawn (its rows past its height).
    pub trash_max: u16,
    /// The help overlay's furthest scroll as drawn (its rows past its height).
    pub help_max: u16,
    /// The workspace list's furthest scroll as drawn (its rows past its height), and the row
    /// it showed first ([`crate::sidebar::render_list`]): the wheel scrolls from there.
    pub ws_max: usize,
    pub ws_start: usize,
    /// The cards (or the grid's tiles) as drawn ([`ListWindow`]).
    pub list: Option<ListWindow>,
    /// The column of cards (the sidebar's, the compact Threads screen's) had cards to show
    /// but no room for one: none of them is selectable ([`crate::app::AppState::nav_order`]).
    pub no_cards: bool,
    /// The compact Threads screen's list as drawn ([`crate::sidebar::render_phone`]): the wheel
    /// over it scrolls it; and what it kept in view, selected then (a key of it).
    pub phone: Option<Rect>,
    pub phone_sel: Option<String>,
    /// The card (or tile) the list as drawn shows first: when cards come or go above it, the
    /// list keeps showing it first rather than the card now at its index.
    pub list_first: Option<String>,
    /// The cards (and tiles) the draw measured, on each list it drew: the heights their text
    /// takes there, from which [`crate::app::ViewState::card_fit`] keeps how tall each is
    /// ([`crate::order::note`]).
    pub fits: Vec<crate::order::Measure>,
    /// The action popup's rows the draw showed, whole, and what it was open on: Enter runs
    /// only one of them ([`crate::app::AppState`]'s `action_key`).
    pub popup_rows: Option<(crate::app::ActionTarget, Vec<crate::app::ActionKind>)>,
    /// On a phone, the first of the action popup's rows the draw showed in its window of them
    /// (its index among those it lists), and what the popup was open on: the next frame keeps
    /// that window while the highlighted row is in it, so that a tap never scrolls the rows
    /// under the finger.
    pub popup_start: Option<(crate::app::ActionTarget, usize)>,
    /// The action popup drew its item counting its detail view read in full for this revision
    /// (every row of it was on screen, [`crate::app::CardView`]), with only some of the options
    /// listed: only an option it drew may be answered.
    pub popup_partial: bool,
    /// The action popup had no room for its item in full: the card (for a request, its
    /// thread) whose detail view the main pane opens for it
    /// ([`crate::app::AppState::note_panes`]).
    pub long_item: Option<String>,
    /// The controls drawn to be tapped ([`Target`]), in drawing order, none under an overlay.
    pub targets: Vec<(Rect, Target)>,
    /// The folded rows of routine updates drawn in the orchestrator conversation, none under
    /// an overlay: what a summary may be asked for ([`crate::summary`]).
    pub routine: Vec<crate::summary::DrawnRun>,
}

/// The list of cards (a column, a row) or the grid of tiles as drawn: where it is and which
/// slice of it shows, so that the wheel over it scrolls it
/// ([`crate::app::ViewState::card_scroll`]) and the next frame keeps that slice while the
/// selection is in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListWindow {
    /// Which list it is.
    pub layout: ListLayout,
    /// The area it takes: its cards or tiles, and the room they leave.
    pub area: Rect,
    /// The first card shown (in the grid, the first row of tiles).
    pub start: usize,
    /// The furthest it scrolls: the first card (row) from which the last ones all show.
    pub max: usize,
}

/// Which list of cards: a first card (row) of one means nothing in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListLayout {
    /// The column of cards beside the main pane.
    Column,
    /// The grid of tiles, this many to a row.
    Grid(usize),
    /// A compact layout's list, one row per card.
    Compact(crate::app::CompactPane),
}

/// A confirm card too long for the orchestrator's banner (or, with no orchestrator, the hub's
/// card), drawn in the main pane as a scrollable detail view (every action on a row of its
/// own, then both options): which rows the draw showed. The card counts as drawn in full ([`DrawnItem`]) only once every row has
/// been on screen and the options are on screen now ([`crate::app::CardView`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardWindow {
    /// The card it shows the confirm card for: the orchestrator's banner, or with no
    /// orchestrator the hub's card.
    pub thread: String,
    /// The card's attention id.
    pub attn_id: String,
    /// Its revision as drawn.
    pub revision: u64,
    /// The width its rows were wrapped at.
    pub width: u16,
    /// How many rows the card has.
    pub rows: usize,
    /// Its rows are the item's whole text: none cut at the width, and not a capped preview
    /// ([`crate::app::AppState::incomplete`]).
    pub complete: bool,
    /// The first row shown.
    pub start: usize,
    /// One past the last row shown.
    pub end: usize,
    /// The furthest it scrolls (rows below the top; `usize`, never saturated: a card of more
    /// than 65,535 rows must still scroll to its options).
    pub max_scroll: usize,
    /// How many rows the view shows at once.
    pub page: u16,
    /// The view's inner area.
    pub area: Rect,
}

impl Panes {
    /// Records `pane` (nothing for an empty area).
    pub fn push(&mut self, pane: Pane) {
        if !pane.area.is_empty() {
            self.panes.push(pane);
        }
    }

    /// Records that `item` was drawn in full.
    pub fn drawn(&mut self, item: DrawnItem) {
        self.items.push(item);
    }

    /// Forgets the items, the moving cells and the controls an overlay drawn over `area`
    /// covers (all of them for `None`: a modal overlay). The overlay records its own controls
    /// after.
    pub fn cover(&mut self, area: Option<Rect>) {
        match area {
            Some(a) => {
                if self.status.as_ref().is_some_and(|(_, r)| r.intersects(a)) {
                    self.status = None;
                }
                self.items.retain(|i| !i.area.intersects(a));
                self.moving.retain(|c| !c.intersects(a));
                self.targets.retain(|(r, _)| !r.intersects(a));
                self.routine.retain(|r| !r.area.intersects(a));
                for w in [&mut self.card, &mut self.sheet, &mut self.ask] {
                    if w.as_ref().is_some_and(|c| c.area.intersects(a)) {
                        *w = None;
                    }
                }
            }
            None => {
                self.status = None;
                self.items.clear();
                self.moving.clear();
                self.targets.clear();
                self.routine.clear();
                self.card = None;
                self.sheet = None;
                self.ask = None;
            }
        }
    }

    /// Records control `target` over `area` (nothing for an empty area).
    pub fn target(&mut self, area: Rect, target: Target) {
        if !area.is_empty() {
            self.targets.push((area, target));
        }
    }

    /// The control at cell `(x, y)`: the one drawn last.
    pub fn target_at(&self, x: u16, y: u16) -> Option<&Target> {
        self.targets
            .iter()
            .rev()
            .find(|(r, _)| r.contains(Position { x, y }))
            .map(|(_, t)| t)
    }

    /// Records a moving cell drawn at `cell` (a working icon's cursor).
    pub fn moves(&mut self, cell: Rect) {
        if !cell.is_empty() {
            self.moving.push(cell);
        }
    }

    /// The item drawn in full on `thread`'s card, if any.
    pub fn drawn_on(&self, thread: &str) -> Option<&DrawnItem> {
        self.items.iter().rev().find(|i| i.thread == thread)
    }

    /// Leaves `cells` of the last pane `id` out of copied text ([`Pane::skip`]).
    pub fn skip(&mut self, id: &PaneId, cells: Rect) {
        if let Some(p) = self.panes.iter_mut().rev().find(|p| p.id == *id) {
            p.skip.push(cells);
        }
    }

    /// Records a pane `id` over `area`.
    pub fn add(&mut self, id: PaneId, area: Rect) {
        self.push(Pane::new(id, area));
    }

    /// The pane at cell `(x, y)`: the one drawn last (an overlay over what is under it).
    pub fn at(&self, x: u16, y: u16) -> Option<&Pane> {
        self.panes.iter().rev().find(|p| p.contains(x, y))
    }

    /// The pane `id` (the last one drawn).
    pub fn get(&self, id: &PaneId) -> Option<&Pane> {
        self.panes.iter().rev().find(|p| p.id == *id)
    }
}

/// A selection inside one pane, from the cell the drag started on (`anchor`) to the cell it
/// is at (`head`), both inside the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The pane: its area as it was when the drag started, its other metadata as of the last
    /// draw ([`Selection::refresh`]).
    pub pane: Pane,
    /// Where the drag started.
    pub anchor: Position,
    /// Where it is now.
    pub head: Position,
    /// The button was released (the text was copied).
    pub done: bool,
}

impl Selection {
    /// A selection of one cell at `(x, y)` in `pane`.
    pub fn start(pane: &Pane, x: u16, y: u16) -> Self {
        let at = clamp(&pane.area, x, y);
        Selection {
            pane: pane.clone(),
            anchor: at,
            head: at,
            done: false,
        }
    }

    /// Moves the end to `(x, y)`, kept inside the pane: above it is the pane's first cell,
    /// below it its last one (as terminals extend a selection dragged past the text).
    pub fn extend(&mut self, x: u16, y: u16) {
        self.head = clamp(&self.pane.area, x, y);
    }

    /// Takes the pane's metadata from the draw `panes` (the one the copied buffer comes
    /// from), keeping the selection's bounds: the cells left out of copied text
    /// ([`Pane::skip`]: a working cursor moves as a reply streams in, so the cells of the draw
    /// the drag started on would cut the new text and copy the new cursor), and, while the
    /// pane keeps its area, how its rows join ([`Pane::joins`]). A pane no longer drawn leaves
    /// nothing out.
    pub fn refresh(&mut self, panes: &Panes) {
        match panes.get(&self.pane.id) {
            Some(p) => {
                self.pane.skip = p.skip.clone();
                if p.area == self.pane.area {
                    self.pane.joins = p.joins.clone();
                }
            }
            None => self.pane.skip.clear(),
        }
    }

    /// Nothing was dragged: a plain click ([`is_tap`]).
    pub fn is_click(&self) -> bool {
        is_tap(self.anchor, self.head)
    }

    /// Start and end, in reading order.
    fn bounds(&self) -> (Position, Position) {
        let (a, b) = (self.anchor, self.head);
        if (a.y, a.x) <= (b.y, b.x) {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// The selected columns of row `y` (inclusive), if it is selected.
    fn row_span(&self, y: u16) -> Option<(u16, u16)> {
        let (s, e) = self.bounds();
        if y < s.y || y > e.y {
            return None;
        }
        let area = self.pane.area;
        let x0 = if y == s.y { s.x } else { area.x };
        let x1 = if y == e.y {
            e.x
        } else {
            area.right().saturating_sub(1)
        };
        Some((x0, x1))
    }

    /// The selected text as drawn in `buf`: a stream from the start cell to the end cell, each
    /// row's trailing spaces trimmed, rows joined by their [`Join`] (a line break unless the
    /// pane wrapped the row), without the empty rows it ends with. A [`Join::Skip`] row adds
    /// nothing, so the rows around it join as if it were not there; an empty row of text is
    /// still a line of its own.
    pub fn text(&self, buf: &Buffer) -> String {
        let (s, e) = self.bounds();
        let mut out = String::new();
        let mut first = true;
        for y in s.y..=e.y {
            let Some((x0, x1)) = self.row_span(y) else {
                continue;
            };
            let join = self.pane.join(y);
            if join == Join::Skip {
                continue;
            }
            if !first {
                match join {
                    Join::Newline => out.push('\n'),
                    Join::Space => out.push(' '),
                    Join::Glue | Join::Skip => {}
                }
            }
            first = false;
            out.push_str(row_text(buf, &self.pane, y, x0, x1).trim_end());
        }
        // Rows dragged over below the text add nothing.
        out.truncate(out.trim_end_matches('\n').len());
        out
    }

    /// Shows the selection: its cells are drawn reversed.
    pub fn highlight(&self, buf: &mut Buffer) {
        let (s, e) = self.bounds();
        for y in s.y..=e.y {
            let Some((x0, x1)) = self.row_span(y) else {
                continue;
            };
            for x in x0..=x1 {
                if buf.area.contains(Position { x, y }) {
                    buf[(x, y)].modifier.toggle(Modifier::REVERSED);
                }
            }
        }
    }
}

/// Whether a press at `down` and its release at `up` are a click (a tap): on the same cell,
/// or the next one on its row. A finger on a touch screen moves a little as it taps; a drag
/// over two cells of one row selects nothing worth copying.
pub fn is_tap(down: Position, up: Position) -> bool {
    down.y == up.y && down.x.abs_diff(up.x) <= 1
}

/// `(x, y)` moved inside `area` (which is not empty).
fn clamp(area: &Rect, x: u16, y: u16) -> Position {
    let (right, bottom) = (
        area.right().saturating_sub(1),
        area.bottom().saturating_sub(1),
    );
    if y < area.y {
        Position {
            x: area.x,
            y: area.y,
        }
    } else if y > bottom {
        Position {
            x: right,
            y: bottom,
        }
    } else {
        Position {
            x: x.clamp(area.x, right),
            y,
        }
    }
}

/// The text of row `y` of `pane` whose cells meet columns `x0..=x1`, without its
/// [`Pane::skip`] cells. A wide character's second cell is not a character of its own: the
/// character counts once, if either of its cells is selected.
fn row_text(buf: &Buffer, pane: &Pane, y: u16, x0: u16, x1: u16) -> String {
    let area = &pane.area;
    let mut out = String::new();
    let mut x = area.x;
    while x < area.right() {
        if !buf.area.contains(Position { x, y }) {
            break;
        }
        let symbol = buf[(x, y)].symbol();
        let w = (width(symbol).max(1)) as u16;
        let last = x.saturating_add(w - 1);
        let skipped = pane.skip.iter().any(|r| r.contains(Position { x, y }));
        if last >= x0 && x <= x1 && !skipped {
            out.push_str(symbol);
        }
        x = x.saturating_add(w);
    }
    out
}

/// The OSC 52 sequence that asks the terminal to put `text` on the clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", twr_core::b64::encode(text.as_bytes()))
}

/// The local clipboard command for this session, from its environment (`env`) and a lookup
/// of programs on `PATH` (`found`): none over SSH (`SSH_CONNECTION` or `SSH_TTY` set, the
/// clipboard is on the other end: OSC 52 reaches it), `pbcopy` on macOS, else `wl-copy` under
/// Wayland, `xclip` or `xsel` under X11.
pub fn clipboard_command(
    env: impl Fn(&str) -> Option<String>,
    found: impl Fn(&str) -> bool,
    macos: bool,
) -> Option<(&'static str, &'static [&'static str])> {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return None;
    }
    let mut candidates: Vec<(&'static str, &'static [&'static str])> = Vec::new();
    if macos {
        candidates.push(("pbcopy", &[]));
    } else {
        if set("WAYLAND_DISPLAY") {
            candidates.push(("wl-copy", &[]));
        }
        if set("DISPLAY") {
            candidates.push(("xclip", &["-selection", "clipboard"]));
            candidates.push(("xsel", &["--clipboard", "--input"]));
        }
    }
    candidates.into_iter().find(|(p, _)| found(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use ratatui::widgets::{Block, Borders, Paragraph};
    use ratatui::Terminal;

    /// Two bordered panes side by side, 12x8 each (10x6 inside), and their panes.
    fn two_panes(left: &[&str], right: &[&str], joins: Vec<Join>) -> (Buffer, Panes) {
        let mut term = Terminal::new(TestBackend::new(24, 8)).unwrap();
        let mut panes = Panes::default();
        term.draw(|f| {
            for (i, text) in [left, right].into_iter().enumerate() {
                let area = Rect::new(i as u16 * 12, 0, 12, 8);
                let block = Block::default().borders(Borders::ALL).title("x");
                let inner = block.inner(area);
                let lines: Vec<Line> = text.iter().map(|t| Line::from(*t)).collect();
                f.render_widget(Paragraph::new(lines).block(block), area);
                let mut pane = Pane::new(
                    if i == 0 {
                        PaneId::Transcript
                    } else {
                        PaneId::Text
                    },
                    inner,
                );
                if i == 0 {
                    pane.joins = joins.clone();
                }
                panes.push(pane);
            }
        })
        .unwrap();
        (term.backend().buffer().clone(), panes)
    }

    #[test]
    fn hit_testing_finds_the_inner_area_only() {
        let (_, panes) = two_panes(&["a"], &["b"], vec![]);
        assert_eq!(panes.at(1, 1).unwrap().id, PaneId::Transcript);
        assert_eq!(panes.at(10, 4).unwrap().id, PaneId::Transcript);
        assert_eq!(panes.at(13, 1).unwrap().id, PaneId::Text);
        // Borders belong to no pane.
        for (x, y) in [(0, 0), (0, 1), (11, 1), (12, 1), (5, 0), (5, 7), (23, 3)] {
            assert!(panes.at(x, y).is_none(), "({x}, {y})");
        }
        // An overlay drawn later wins.
        let mut panes = panes;
        panes.add(PaneId::Help, Rect::new(5, 2, 10, 2));
        assert_eq!(panes.at(6, 2).unwrap().id, PaneId::Help);
        assert_eq!(panes.at(6, 1).unwrap().id, PaneId::Transcript);
        // Empty areas are not recorded.
        panes.add(PaneId::Input, Rect::new(0, 0, 0, 3));
        assert!(panes.get(&PaneId::Input).is_none());
    }

    #[test]
    fn a_drag_selects_inside_its_pane_without_borders() {
        let (buf, panes) = two_panes(&["one two", "three", "four"], &["other", "pane"], vec![]);
        let pane = panes.at(3, 1).unwrap();
        // From "two" on the first row, dragged right across the border into the other pane
        // and down: clipped to the left pane, rows joined by line breaks, trailing spaces gone.
        let mut sel = Selection::start(pane, 5, 1);
        sel.extend(20, 2);
        assert_eq!(sel.head, Position { x: 10, y: 2 });
        assert_eq!(sel.text(&buf), "two\nthree");
        // Dragged below the pane: to its last cell (the empty rows add nothing).
        sel.extend(3, 9);
        assert_eq!(sel.head, Position { x: 10, y: 6 });
        assert_eq!(sel.text(&buf), "two\nthree\nfour");
        // Backwards (end before start) reads the same way.
        let mut back = Selection::start(pane, 3, 2);
        back.extend(5, 1);
        assert_eq!(back.text(&buf), "two\nthr");
        // Above the pane: from its first cell.
        let mut up = Selection::start(pane, 4, 2);
        up.extend(4, 0);
        assert_eq!(up.text(&buf), "one two\nthre");
        // A click is a one-cell selection.
        let click = Selection::start(pane, 1, 1);
        assert!(click.is_click());
        assert_eq!(click.text(&buf), "o");
    }

    #[test]
    fn soft_wrapped_rows_join_without_a_line_break() {
        // "alpha beta" wrapped at a space, then "gammadelta" split mid-word, then a new line.
        let (buf, panes) = two_panes(
            &["alpha", "beta", "gammadelta", "xyz", "next"],
            &[],
            vec![
                Join::Newline,
                Join::Space,
                Join::Newline,
                Join::Glue,
                Join::Newline,
            ],
        );
        let mut sel = Selection::start(panes.at(1, 1).unwrap(), 1, 1);
        sel.extend(10, 4);
        assert_eq!(sel.text(&buf), "alpha beta\ngammadeltaxyz");
    }

    #[test]
    fn highlight_reverses_the_selected_cells_only() {
        let (mut buf, panes) = two_panes(&["abc", "def"], &["x"], vec![]);
        let mut sel = Selection::start(panes.at(2, 1).unwrap(), 2, 1);
        sel.extend(1, 2);
        sel.highlight(&mut buf);
        let rev = |x: u16, y: u16| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        assert!(!rev(1, 1) && rev(2, 1) && rev(10, 1));
        assert!(rev(1, 2) && !rev(2, 2));
        assert!(
            !rev(0, 1) && !rev(11, 1) && !rev(13, 1),
            "no border, no other pane"
        );
    }

    #[test]
    fn wide_characters_are_copied_once() {
        let (buf, panes) = two_panes(&["日本語 ok"], &[], vec![]);
        let pane = panes.at(1, 1).unwrap();
        // Cells 1-2 hold 日, 3-4 本, 5-6 語. Starting on 本's second cell still takes 本.
        let mut sel = Selection::start(pane, 4, 1);
        sel.extend(6, 1);
        assert_eq!(sel.text(&buf), "本語");
        let mut sel = Selection::start(pane, 1, 1);
        sel.extend(10, 1);
        assert_eq!(sel.text(&buf), "日本語 ok");
    }

    #[test]
    fn base64_and_osc52() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(twr_core::b64::encode(plain.as_bytes()), encoded, "{plain}");
        }
        assert_eq!(twr_core::b64::encode(&[0xff, 0xfe, 0x00]), "//4A");
        assert_eq!(twr_core::b64::encode("é".as_bytes()), "w6k=");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn the_clipboard_command_follows_the_session() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let all = |_: &str| true;
        let cmd = |pairs, found: &dyn Fn(&str) -> bool, mac| {
            clipboard_command(env(pairs), found, mac).map(|c| c.0)
        };
        assert_eq!(cmd(&[], &all, true), Some("pbcopy"));
        assert_eq!(cmd(&[("SSH_TTY", "/dev/pts/1")], &all, true), None);
        assert_eq!(cmd(&[("SSH_CONNECTION", "1 2 3 4")], &all, false), None);
        assert_eq!(cmd(&[("SSH_TTY", "")], &all, true), Some("pbcopy"));
        assert_eq!(cmd(&[], &all, false), None, "no display: OSC 52 only");
        assert_eq!(
            cmd(
                &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")],
                &all,
                false
            ),
            Some("wl-copy")
        );
        assert_eq!(cmd(&[("DISPLAY", ":0")], &all, false), Some("xclip"));
        assert_eq!(
            cmd(&[("DISPLAY", ":0")], &|p: &str| p == "xsel", false),
            Some("xsel")
        );
        assert_eq!(cmd(&[("DISPLAY", ":0")], &|_: &str| false, false), None);
        assert_eq!(
            clipboard_command(env(&[("DISPLAY", ":0")]), all, false),
            Some(("xclip", &["-selection", "clipboard"][..]))
        );
    }
}
