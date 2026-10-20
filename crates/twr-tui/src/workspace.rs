//! Workspaces (`twr ws`): separate hubs on this machine, which the TUI switches between, one
//! connected at a time. The tab strip ([`crate::ui`]) lists them once there is more than
//! `default`; Alt-1..9, Ctrl-PgUp/PgDn, a click on a tab and `/ws <name>` switch. A switch
//! keeps the view of the workspace left ([`SavedView`]) and restores it on coming back.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use twr_core::paths::workspace_name_error;
use twr_core::reduce::ThreadStatus;

use crate::app::{Action, AppState, Confirm, DecisionFilter, Screen, Transcript};
use crate::edit::LineInput;
use crate::notify::{Notifier, WorkspaceAlert};
use crate::select::Anchor;

/// A workspace in the tab strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    /// Its name.
    pub name: String,
    /// What its hub said when last asked (the current workspace's comes from the state
    /// itself, [`AppState::tab_hub`]).
    pub hub: TabHub,
}

/// A workspace's hub, as the tab strip shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabHub {
    /// Not asked yet.
    #[default]
    Unknown,
    /// Nothing listens on its socket: `○`.
    Stopped,
    /// Something listens but did not answer in time.
    Busy,
    /// It answered: its worker threads, how many run, and the permissions and questions
    /// waiting on the user.
    Running {
        threads: usize,
        running: usize,
        permissions: usize,
        questions: usize,
    },
}

impl TabHub {
    /// The items waiting on the user (its `⚠` badge): permissions and questions.
    pub fn waiting(&self) -> usize {
        match self {
            TabHub::Running {
                permissions,
                questions,
                ..
            } => permissions + questions,
            _ => 0,
        }
    }
}

/// What a workspace's view keeps while another workspace is shown: where the user was (the
/// selected card, the dive, the screen, the scroll) and what they typed, and the transcripts
/// it had, so coming back resumes them where they ended and the scroll lands on the same
/// rows. The registry and attention come from its hub's log, replayed again.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SavedView {
    input: LineInput,
    screen: Screen,
    back: Screen,
    decision_filter: DecisionFilter,
    expand_all: bool,
    long_expanded: HashSet<String>,
    show_parked: bool,
    sidebar_hidden: bool,
    sidebar_focus: Option<String>,
    focus_mode: bool,
    /// The compact layout's screen.
    compact_pane: crate::app::CompactPane,
    /// The cards as shown ([`crate::order`]): coming back, they are where they were, and a
    /// move that waited still waits for the selection restored with them.
    shown_cards: Option<crate::order::Shown>,
    restoring: Restoring,
    /// What the view followed of the messages it sent, to take one back
    /// ([`AppState::sent`], [`AppState::withdrawals`], ...): a call's answer that comes after
    /// the switch is applied to it once the workspace shows again.
    sent: Option<crate::app::Sent>,
    withdrawals: Vec<crate::app::Withdrawal>,
    started_refs: std::collections::VecDeque<(String, i64, u64)>,
    acted_turns: std::collections::VecDeque<(String, u64)>,
    /// The history entries too long for the history file (a message taken back,
    /// [`AppState::keep_taken_back`]): kept for the session, they come back after the file's.
    session_history: Vec<String>,
    /// Answers to its messages that came while it was being restored
    /// ([`AppState::deferred`]): applied once it is.
    deferred: Vec<(&'static str, serde_json::Value)>,
    /// Its `!` commands' state ([`crate::shell::Saved`]): the folder they run in, the `!cd`s
    /// and launches on their way, the commands Ctrl-C stops.
    shell: crate::shell::Saved,
}

/// What of a [`SavedView`] waits until the replay is applied ([`AppState::finish_restore`]):
/// the transcripts (kept only for threads still there: the workspace may have been reset or
/// made again meanwhile), the dive (its thread may be gone), the main pane's scroll and
/// opened groups, which the replay's own changes of the main pane reset, and the decision
/// table's selection, which each replayed decision moves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Restoring {
    /// Pins: a replayed deletion of a thread restored since would drop its pin. Restored
    /// before the selected card, which a pin may be what lists.
    pins: HashSet<String>,
    focus: Option<String>,
    /// The conversation selected (`←`) rather than the cards: what a request selected there
    /// and the arrows go by.
    main_focus: bool,
    /// The request selected in the conversation, and the confirm card's detail view (its
    /// scroll), while their items are still open.
    request: Option<String>,
    card_view: crate::app::CardView,
    /// The card whose item the main pane showed as its detail view, while that item is still
    /// open: it keeps the card selected, also where the view draws no card for it.
    detail: Option<String>,
    /// The selected decision's event id, which keeps its row when decisions came meanwhile.
    decision_id: Option<String>,
    transcript: Option<Transcript>,
    transcripts: Vec<Transcript>,
    dive: Option<String>,
    focus_before_dive: Option<String>,
    scroll: usize,
    anchor: Option<Anchor>,
    expanded: HashSet<String>,
    decision_sel: usize,
}

/// How a `twr attach` command shown in the TUI (`/attach`) reaches the hub it shows: through
/// the socket it was opened on (`explicit`, `--socket`), else in its workspace (`-w`, for a
/// named one, and for `default` when the shell selects another, `inherited`
/// `TWR_WORKSPACE`): ` --socket '<path>'`, ` -w <name>` or nothing.
pub fn attach_via(
    explicit: Option<&std::path::Path>,
    workspace: Option<&str>,
    inherited: Option<&std::ffi::OsStr>,
) -> String {
    match explicit {
        Some(socket) => format!(" --socket {}", shell_quote(&socket.to_string_lossy())),
        None => twr_core::paths::workspace_selection(workspace, inherited),
    }
}

/// `s` as one shell word: as it is when it has only characters no shell treats specially,
/// else in single quotes.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '/' | '.' | '_' | '-' | ':' | '@' | '+' | '=' | ',')
        });
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// What the tab poll's `tabs` say to the user beyond the strip: the alerts ([`Notifier`]) for
/// the workspaces other than `current` whose count of items waiting on the user went up
/// since `seen` (by workspace, updated). One the notifier holds back for now
/// ([`WorkspaceAlert::Later`]) keeps the count it was compared with, so the next poll asks
/// again (and says nothing if the count went back down meanwhile).
pub fn waiting_alerts(
    seen: &mut HashMap<String, usize>,
    tabs: &[Tab],
    current: Option<&str>,
    notifier: &mut Notifier,
    now: Instant,
) -> Vec<String> {
    let mut out = Vec::new();
    for tab in tabs {
        let TabHub::Running {
            permissions,
            questions,
            ..
        } = tab.hub
        else {
            continue;
        };
        if Some(tab.name.as_str()) == current {
            continue;
        }
        // Of the kinds the user is notified of.
        let waiting = notifier.waiting_count(permissions, questions);
        let before = seen.insert(tab.name.clone(), waiting);
        if !before.is_some_and(|b| waiting > b) {
            continue;
        }
        match notifier.workspace_waiting(&tab.name, waiting, now) {
            WorkspaceAlert::Send(bytes) => out.push(bytes),
            WorkspaceAlert::Seen => {}
            WorkspaceAlert::Later => {
                if let Some(b) = before {
                    seen.insert(tab.name.clone(), b);
                }
            }
        }
    }
    out
}

impl AppState {
    /// Whether the tab strip is drawn: switching is possible and there is more than one
    /// workspace.
    pub fn tabs_shown(&self) -> bool {
        self.workspace.is_some() && self.tabs.len() > 1
    }

    /// The instance label the views draw (`[name]`): none while the tab strip names it.
    pub fn instance_label(&self) -> Option<&str> {
        if self.tabs_shown() {
            None
        } else {
            self.instance_name.as_deref()
        }
    }

    /// What tab `tab` shows: the current workspace's hub as the state has it now, the
    /// others as last asked.
    pub fn tab_hub(&self, tab: &Tab) -> TabHub {
        if self.workspace.as_deref() != Some(tab.name.as_str()) {
            return tab.hub;
        }
        let workers: Vec<_> = self
            .registry
            .list()
            .into_iter()
            .filter(|t| !t.is_orchestrator())
            .collect();
        let running = workers
            .iter()
            .filter(|t| matches!(t.status(), ThreadStatus::Running | ThreadStatus::Background))
            .count();
        let items = self.attention.ordered(&HashSet::new());
        let of = |reason| items.iter().filter(|i| i.reason == reason).count();
        TabHub::Running {
            threads: workers.len(),
            running,
            permissions: of(twr_core::event::AttentionReason::Permission),
            questions: of(twr_core::event::AttentionReason::Question),
        }
    }

    /// Sets the tabs to `tabs` (their order is the strip's), keeping the current workspace
    /// listed even if the list missed it.
    pub fn set_tabs(&mut self, mut tabs: Vec<Tab>) {
        if let Some(current) = &self.workspace {
            if !tabs.iter().any(|t| &t.name == current) {
                tabs.push(Tab {
                    name: current.clone(),
                    hub: TabHub::Unknown,
                });
            }
        }
        self.tabs = tabs;
    }

    /// Switches to workspace `name` (`/ws <name>`, a click on its tab): one that exists is
    /// opened; another, valid name asks first whether to create it.
    pub fn switch_workspace(&mut self, name: &str) -> Vec<Action> {
        let Some(current) = self.workspace.clone() else {
            self.view.status =
                Some("workspaces: not here (this TUI was opened on a socket)".into());
            return Vec::new();
        };
        if let Some(to) = &self.switching {
            self.view.status = Some(format!("already opening workspace {to}"));
            return Vec::new();
        }
        if name == current {
            self.view.status = Some(format!("already in workspace {name}"));
            return Vec::new();
        }
        if self.tabs.iter().any(|t| t.name == name) {
            self.view.status = Some(format!("opening workspace {name}..."));
            self.switching = Some(name.to_string());
            // A question left waiting for `y` (another workspace to create, say) goes: its
            // answer would start a second switch.
            self.view.confirm = None;
            return vec![Action::Workspace {
                name: name.to_string(),
                create: false,
            }];
        }
        if let Some(why) = workspace_name_error(name) {
            self.view.status = Some(why);
            return Vec::new();
        }
        self.view.confirm = Some(Confirm {
            action: Action::Workspace {
                name: name.to_string(),
                create: true,
            },
            prompt: format!("no workspace {name}: create it and switch to it? y/n"),
            pending: format!("creating workspace {name}..."),
            alt: None,
        });
        Vec::new()
    }

    /// The IO loop is to open workspace `name` ([`Action::Workspace`]): `false` when another
    /// one is being opened already (the status says so), so one switch runs at a time.
    pub fn begin_switch(&mut self, name: &str) -> bool {
        match &self.switching {
            Some(other) if other != name => {
                self.view.status = Some(format!("already opening workspace {other}"));
                false
            }
            _ => {
                self.switching = Some(name.to_string());
                true
            }
        }
    }

    /// What `/ws` says: the workspaces, the current one marked, and how to switch.
    pub fn workspaces_line(&self) -> String {
        if self.workspace.is_none() {
            return "workspaces: not here (this TUI was opened on a socket)".into();
        }
        let names: Vec<String> = self
            .tabs
            .iter()
            .map(|t| {
                if self.workspace.as_deref() == Some(t.name.as_str()) {
                    format!("*{}", t.name)
                } else {
                    t.name.clone()
                }
            })
            .collect();
        format!(
            "workspaces: {} (/ws <name> switches or creates one)",
            names.join(", ")
        )
    }

    /// Switches to the tab at `index` (Alt-1 is 0); nothing when there is none.
    pub fn switch_tab(&mut self, index: usize) -> Vec<Action> {
        match self.tabs.get(index).map(|t| t.name.clone()) {
            Some(name) if self.tabs_shown() => self.switch_workspace(&name),
            _ => Vec::new(),
        }
    }

    /// Switches to the next tab (`forward`) or the previous one, around the ends.
    pub fn step_tab(&mut self, forward: bool) -> Vec<Action> {
        if !self.tabs_shown() {
            return Vec::new();
        }
        let n = self.tabs.len();
        let at = self
            .tabs
            .iter()
            .position(|t| Some(&t.name) == self.workspace.as_ref())
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        };
        self.switch_tab(next)
    }

    /// The view to keep for this workspace while another is shown (the state is left
    /// without its transcripts: it is done with).
    pub fn take_view(&mut self) -> SavedView {
        let shell = self.take_shell();
        let v = &self.view;
        // Up on the input line shows a history entry (unedited): what the user had typed is
        // the line to keep.
        let input = match self.browsing() {
            Some(_) => {
                let mut draft = LineInput::default();
                draft.set(&v.history_draft);
                draft
            }
            None => v.input.clone(),
        };
        SavedView {
            input,
            screen: v.screen,
            back: v.back,
            decision_filter: v.decision_filter,
            expand_all: v.expand_all,
            long_expanded: v.long_expanded.clone(),
            show_parked: v.show_parked,
            sidebar_hidden: v.sidebar_hidden,
            sidebar_focus: v.sidebar_focus.clone(),
            focus_mode: v.focus_mode,
            compact_pane: v.compact_pane,
            shown_cards: v.shown_cards.clone(),
            sent: self.sent.take().map(|mut s| {
                // Its hint does not outlive the view.
                s.armed = None;
                s
            }),
            withdrawals: std::mem::take(&mut self.withdrawals),
            started_refs: std::mem::take(&mut self.started_refs),
            acted_turns: std::mem::take(&mut self.acted_turns),
            deferred: std::mem::take(&mut self.deferred),
            shell,
            session_history: v
                .history
                .iter()
                .filter(|h| h.len() > crate::HISTORY_ENTRY_MAX)
                .cloned()
                .collect(),
            restoring: Restoring {
                pins: v.pins.clone(),
                focus: v.focus.clone(),
                main_focus: v.main_focus,
                request: v.request.clone(),
                card_view: v.card_view.clone(),
                detail: v.detail.clone(),
                decision_id: self
                    .decision_rows()
                    .get(self.decision_selected())
                    .map(|r| r.id.clone()),
                dive: v.dive.clone(),
                focus_before_dive: v.focus_before_dive.clone(),
                scroll: v.scroll,
                anchor: v.anchor.clone(),
                expanded: v.expanded.clone(),
                decision_sel: v.decision_sel,
                transcript: self.transcript.take(),
                transcripts: std::mem::take(&mut self.transcripts),
            },
        }
    }

    /// Puts back the view kept when the user left this workspace, before its log is replayed;
    /// the rest once it is applied ([`AppState::finish_restore`]).
    pub fn restore_view(&mut self, saved: SavedView) {
        let v = &mut self.view;
        v.input = saved.input;
        v.screen = saved.screen;
        v.back = saved.back;
        v.decision_filter = saved.decision_filter;
        v.expand_all = saved.expand_all;
        v.long_expanded = saved.long_expanded;
        v.show_parked = saved.show_parked;
        v.sidebar_hidden = saved.sidebar_hidden;
        v.sidebar_focus = saved.sidebar_focus;
        v.focus_mode = saved.focus_mode;
        v.compact_pane = saved.compact_pane;
        v.shown_cards = saved.shown_cards;
        self.sent = saved.sent;
        self.withdrawals = saved.withdrawals;
        self.started_refs = saved.started_refs;
        self.acted_turns = saved.acted_turns;
        self.deferred = saved.deferred;
        self.restore_shell(saved.shell);
        // The file's history goes before them as it is read ([`AppState::load_history`]).
        self.view.history = saved.session_history;
        self.restoring = Some(saved.restoring);
    }

    /// The replay is applied: the transcripts kept for threads still there replace the ones
    /// the replay started (theirs resume where they ended), the dive kept for this workspace
    /// resumes when its thread is still there (and the stack view is shown), and the main
    /// pane's scroll, opened groups and decision selection come back. When the dive's thread
    /// is gone, the conversation shows from its bottom. Then the answers to this workspace's
    /// messages that came meanwhile are applied ([`AppState::deferred`]): a message taken back
    /// goes into the box of the view as it was left.
    pub(crate) fn finish_restore(&mut self) {
        if self.restoring.is_none() {
            return;
        }
        self.restore_rest();
        for (method, result) in std::mem::take(&mut self.deferred) {
            self.apply_result(method, &result);
        }
    }

    /// [`AppState::finish_restore`] but for the answers that waited.
    fn restore_rest(&mut self) {
        let Some(r) = self.restoring.take() else {
            return;
        };
        let live = |t: &Transcript| self.registry.get(&t.thread).is_some();
        let mut kept: Vec<Transcript> = r.transcripts.into_iter().filter(live).collect();
        kept.extend(r.transcript.filter(live));
        if let Some(main) = self.main_thread() {
            if let Some(i) = kept.iter().position(|t| t.thread == main) {
                self.transcript = Some(kept.remove(i));
            }
        }
        let over = kept.len().saturating_sub(crate::app::TRANSCRIPT_CACHE);
        kept.drain(..over);
        self.transcripts = kept;
        // The dive first: it releases the selection, which the selection kept replaces below.
        let mut dive_gone = false;
        if let Some(thread) = r.dive {
            let there = self
                .registry
                .get(&thread)
                .is_some_and(|t| !t.is_orchestrator());
            // Under the decisions overlay, the stack view it returns to had the dive.
            let overlay = self.view.screen == Screen::Decisions;
            if there
                && (self.view.screen == Screen::Stack || overlay && self.view.back == Screen::Stack)
            {
                // The compact screen left on stays: the dive goes on under it.
                let pane = self.view.compact_pane;
                self.dive(&thread);
                self.view.compact_pane = pane;
                if overlay {
                    self.view.screen = Screen::Decisions;
                }
                self.view.focus_before_dive = r.focus_before_dive;
            } else {
                dive_gone = true;
            }
        }
        self.view.pins = r
            .pins
            .into_iter()
            .filter(|p| p == twr_core::event::HUB_THREAD || self.registry.get(p).is_some())
            .collect();
        self.view.focus = r.focus;
        self.view.main_focus = r.main_focus;
        self.view.request = r.request.filter(|id| self.attention.get(id).is_some());
        // Before the selection is checked against what the view draws (`ensure_focus`, once
        // this returns): the detail's card counts as drawn.
        self.view.detail = r.detail.filter(|card| !self.card_items(card).is_empty());
        let card = &r.card_view;
        if self
            .attention
            .get(&card.attn_id)
            .is_some_and(|i| i.revision == card.revision)
        {
            self.view.card_view = r.card_view;
        }
        self.view.decision_sel = r.decision_sel;
        if let Some(id) = r.decision_id {
            if let Some(i) = self.decision_rows().iter().position(|row| row.id == id) {
                self.view.decision_sel = i;
            }
        }
        // The conversation shows from its bottom when the thread dived into is gone.
        if dive_gone {
            return;
        }
        self.view.scroll = r.scroll;
        self.view.anchor = r.anchor;
        self.view.expanded = r.expanded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn tabs(names: &[&str]) -> Vec<Tab> {
        names
            .iter()
            .map(|n| Tab {
                name: n.to_string(),
                hub: TabHub::Stopped,
            })
            .collect()
    }

    fn in_workspaces(current: &str, names: &[&str]) -> AppState {
        let mut st = AppState::new();
        st.workspace = Some(current.into());
        st.set_tabs(tabs(names));
        st
    }

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    fn switch(name: &str, create: bool) -> Vec<Action> {
        vec![Action::Workspace {
            name: name.into(),
            create,
        }]
    }

    #[test]
    fn tabs_show_only_with_switching_and_another_workspace() {
        let mut st = AppState::new();
        st.instance_name = Some("default".into());
        assert!(!st.tabs_shown());
        assert_eq!(st.instance_label(), Some("default"));
        st.workspace = Some("default".into());
        st.set_tabs(tabs(&["default"]));
        assert!(!st.tabs_shown());
        st.set_tabs(tabs(&["default", "a"]));
        assert!(st.tabs_shown());
        assert_eq!(st.instance_label(), None, "the strip names it");
        // The current workspace stays listed.
        st.workspace = Some("b".into());
        st.set_tabs(tabs(&["default", "a"]));
        assert_eq!(st.tabs.last().unwrap().name, "b");
    }

    #[test]
    fn alt_digits_and_ctrl_page_keys_switch_tabs() {
        let alt = KeyModifiers::ALT;
        let ctrl = KeyModifiers::CONTROL;
        let mut st = in_workspaces("a", &["default", "a", "b"]);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('3'), alt)),
            switch("b", false)
        );
        assert_eq!(st.switching.as_deref(), Some("b"));
        // One switch at a time.
        assert!(st.handle_key(key(KeyCode::Char('1'), alt)).is_empty());
        assert_eq!(
            st.view.status.as_deref(),
            Some("already opening workspace b")
        );

        let mut st = in_workspaces("a", &["default", "a", "b"]);
        assert!(st.handle_key(key(KeyCode::Char('2'), alt)).is_empty());
        assert_eq!(st.view.status.as_deref(), Some("already in workspace a"));
        assert!(st.handle_key(key(KeyCode::Char('9'), alt)).is_empty());
        assert_eq!(
            st.handle_key(key(KeyCode::PageDown, ctrl)),
            switch("b", false)
        );
        let mut st = in_workspaces("default", &["default", "a", "b"]);
        assert_eq!(
            st.handle_key(key(KeyCode::PageUp, ctrl)),
            switch("b", false),
            "around the start"
        );
        // They type nothing.
        let mut st = in_workspaces("default", &["default", "a"]);
        assert_eq!(
            st.handle_key(key(KeyCode::Char('2'), alt)),
            switch("a", false)
        );
        assert_eq!(st.view.input.as_str(), "");
        // Without the strip they do nothing.
        let mut st = in_workspaces("default", &["default"]);
        assert!(st.handle_key(key(KeyCode::PageDown, ctrl)).is_empty());
        assert!(st.handle_key(key(KeyCode::Char('1'), alt)).is_empty());
    }

    #[test]
    fn a_new_workspace_is_created_only_once_confirmed() {
        let mut st = in_workspaces("default", &["default", "a"]);
        assert!(st.switch_workspace("new-one").is_empty());
        let c = st.view.confirm.clone().unwrap();
        assert!(
            c.prompt.contains("no workspace new-one: create it"),
            "{c:?}"
        );
        assert_eq!(
            st.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            switch("new-one", true)
        );
        let mut st = in_workspaces("default", &["default", "a"]);
        assert!(st.switch_workspace("Bad").is_empty());
        assert!(st.view.confirm.is_none());
        assert!(st
            .view
            .status
            .as_deref()
            .unwrap()
            .contains("invalid workspace name"));
        // A TUI opened on a socket has no workspaces to switch to.
        let mut st = AppState::new();
        assert!(st.switch_workspace("a").is_empty());
        assert!(st.view.status.as_deref().unwrap().contains("not here"));
    }

    /// Back in a workspace, the dive it was left in resumes once the replay shows the thread,
    /// with what the user was typing to it.
    #[test]
    fn a_dive_resumes_after_the_replay() {
        let mut st = crate::app::tests::three_threads();
        st.dive("t-beta");
        st.view.input.set("draft for beta");
        let saved = st.take_view();

        let mut back = crate::app::tests::three_threads();
        back.restore_view(saved);
        back.finish_restore();
        assert_eq!(back.view.dive.as_deref(), Some("t-beta"));
        assert_eq!(back.main_thread().as_deref(), Some("t-beta"));
        assert_eq!(back.view.input.as_str(), "draft for beta");
    }

    /// With `/compact on`, a workspace left on a list during a dive comes
    /// back on that list, the dive under it.
    #[test]
    fn a_compact_list_left_in_a_dive_comes_back() {
        use crate::app::CompactPane;
        let mut st = crate::app::tests::three_threads();
        st.view.compact = Some(true);
        st.dive("t-beta");
        st.view.compact_pane = CompactPane::Threads;
        let saved = st.take_view();

        let mut back = crate::app::tests::three_threads();
        back.view.compact = Some(true);
        back.restore_view(saved);
        back.finish_restore();
        assert_eq!(back.view.dive.as_deref(), Some("t-beta"));
        assert_eq!(back.view.compact_pane, CompactPane::Threads);
    }

    #[test]
    fn ws_parses_a_name_or_none() {
        use crate::slash::{parse, Slash};
        assert_eq!(parse("/ws"), Ok(Slash::Ws(None)));
        assert_eq!(parse("/ws a"), Ok(Slash::Ws(Some("a".into()))));
        assert_eq!(parse("/workspace a"), Ok(Slash::Ws(Some("a".into()))));
        assert!(parse("/ws a b").is_err());
        let mut st = in_workspaces("a", &["default", "a"]);
        st.view.input.set("/ws");
        // The popup closed (Esc), so Enter runs the line.
        st.view.popup_closed = Some("/ws".into());
        let _ = st.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            st.view.status.as_deref(),
            Some("workspaces: default, *a (/ws <name> switches or creates one)")
        );
    }

    /// The first-run picker of a new workspace is modal, but not for switching workspaces.
    #[test]
    fn the_first_run_picker_lets_the_user_switch_away() {
        let mut st = in_workspaces("a", &["default", "a"]);
        st.view.picker = Some(crate::app::Picker {
            runtimes: vec![("claude-code".into(), Some(true))],
            selected: 0,
            default: "claude-code".into(),
            model: None,
            switch: false,
        });
        assert!(st.picker_open());
        assert_eq!(
            st.handle_key(key(KeyCode::Char('1'), KeyModifiers::ALT)),
            switch("default", false)
        );
        let mut st2 = st.clone();
        st2.switching = None;
        assert_eq!(
            st2.handle_key(key(KeyCode::PageUp, KeyModifiers::CONTROL)),
            switch("default", false)
        );
    }

    /// A click on a tab switches to its workspace.
    #[test]
    fn a_click_on_a_tab_switches() {
        use crate::select::PaneId;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut st = in_workspaces("default", &["default", "a"]);
        let screen = crate::app::tests::draw(&mut st, 100, 30);
        let tab = st
            .view
            .panes
            .get(&PaneId::Tab("a".into()))
            .expect("the tab's pane")
            .area;
        let at = |kind| MouseEvent {
            kind,
            column: tab.x + 1,
            row: tab.y,
            modifiers: KeyModifiers::NONE,
        };
        let left = MouseButton::Left;
        assert!(st
            .handle_mouse(at(MouseEventKind::Down(left)), &screen)
            .is_empty());
        assert_eq!(
            st.handle_mouse(at(MouseEventKind::Up(left)), &screen),
            switch("a", false)
        );
    }

    #[test]
    fn a_saved_view_comes_back_and_its_dive_waits_for_the_replay() {
        let mut st = AppState::new();
        st.view.input.set("half a line");
        st.view.screen = Screen::Stack;
        st.view.pins.insert("t1".into());
        st.view.dive = Some("t1".into());
        st.view.sidebar_hidden = true;
        let saved = st.take_view();

        let mut back = AppState::new();
        back.restore_view(saved.clone());
        assert_eq!(back.view.input.as_str(), "half a line");
        assert!(back.view.sidebar_hidden);
        assert_eq!(back.view.dive, None, "not before the replay");
        // The thread is gone: no dive, and no pin.
        back.finish_restore();
        assert_eq!(back.view.dive, None);
        assert!(back.view.pins.is_empty());
        assert_eq!(back.restoring, None);
    }

    /// The main pane's scroll and opened groups survive the replay of the log, whose own
    /// changes of the main pane (the orchestrator appearing) reset them; the transcript comes
    /// along, so its rows are there at once.
    #[test]
    fn the_scroll_and_opened_groups_come_back_after_the_replay() {
        use crate::app::tests::{control, orch_events, thread_item};
        use twr_core::testkit::ThreadScript;
        let mut o = ThreadScript::new("t-orch");
        o.at(40);
        let evs = orch_events(&mut o, "t-orch");
        let mut st = AppState::new();
        control(&mut st, &evs);
        for i in 0..30 {
            st.apply_event(&thread_item(&o.text(&format!("reply {i}"))));
        }
        st.view.scroll = 7;
        st.view.anchor = Some(Anchor::default());
        st.view.expanded.insert("group-1".into());
        let saved = st.take_view();

        let mut back = AppState::new();
        back.restore_view(saved);
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        assert!(back.catch_up.is_none());
        assert_eq!(back.view.scroll, 7);
        assert_eq!(back.view.anchor, Some(Anchor::default()));
        assert!(back.view.expanded.contains("group-1"));
        assert_eq!(back.transcript.as_ref().unwrap().thread, "t-orch");
    }

    /// Switching while Up shows a history entry keeps what the user had typed.
    #[test]
    fn a_draft_behind_the_history_is_kept() {
        let mut st = in_workspaces("default", &["default", "a"]);
        st.load_history(vec!["an old line".into()]);
        st.view.input.set("my draft");
        assert!(st.history_prev());
        assert_eq!(st.view.input.as_str(), "an old line");
        let saved = st.take_view();
        let mut back = in_workspaces("default", &["default", "a"]);
        back.restore_view(saved);
        assert_eq!(back.view.input.as_str(), "my draft");
    }

    /// One switch at a time: a switch drops a question left waiting for `y`, and a second
    /// open is refused while one runs.
    #[test]
    fn a_switch_drops_a_pending_question_and_one_runs_at_a_time() {
        let mut st = in_workspaces("default", &["default", "a", "b"]);
        assert!(st.switch_workspace("new-one").is_empty());
        assert!(st.view.confirm.is_some());
        assert_eq!(
            st.handle_key(key(KeyCode::Char('2'), KeyModifiers::ALT)),
            switch("a", false)
        );
        assert!(
            st.view.confirm.is_none(),
            "its y would start a second switch"
        );
        assert!(st.begin_switch("a"));
        assert!(!st.begin_switch("new-one"));
        assert_eq!(
            st.view.status.as_deref(),
            Some("already opening workspace a")
        );
        // Once it failed, another may start.
        st.switching = None;
        assert!(st.begin_switch("new-one"));
    }

    /// `/attach` reaches the hub shown: in its workspace, or through the socket the TUI was
    /// opened on.
    #[test]
    fn attach_reaches_the_hub_shown() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;
        let a = Some(OsStr::new("a"));
        assert_eq!(attach_via(None, Some("a"), None), " -w a");
        assert_eq!(attach_via(None, Some("default"), None), "");
        assert_eq!(attach_via(None, None, None), "");
        // The shell selects another workspace (one not UTF-8 too): default must be named.
        assert_eq!(attach_via(None, Some("default"), a), " -w default");
        assert_eq!(
            attach_via(None, Some("default"), Some(OsStr::from_bytes(b"a\xff"))),
            " -w default"
        );
        assert_eq!(
            attach_via(None, Some("default"), Some(OsStr::new("default"))),
            ""
        );
        assert_eq!(attach_via(None, Some("b"), a), " -w b");
        assert_eq!(
            attach_via(Some(Path::new("/tmp/custom.sock")), Some("a"), None),
            " --socket /tmp/custom.sock"
        );
        assert_eq!(
            attach_via(Some(Path::new("/tmp/my hub's.sock")), None, None),
            " --socket '/tmp/my hub'\\''s.sock'"
        );
        let mut st = crate::app::tests::three_threads();
        st.attach_via = attach_via(None, Some("a"), None);
        st.view.input.set("/attach beta");
        st.view.popup_closed = Some("/attach beta".into());
        let _ = st.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            st.view.status.as_deref(),
            Some("to attach, run in another terminal: twr -w a attach beta")
        );
    }

    /// Two workspaces whose waiting count rises in one poll: the first alerts, the second is
    /// held back by the gap between alerts and alerts at a later poll, unless its count went
    /// back down meanwhile.
    #[test]
    fn a_held_back_workspace_alert_comes_at_a_later_poll() {
        use twr_core::config::{NotifyWhen, TerminalAlert, TuiNotifyConfig};
        let cfg = TuiNotifyConfig {
            terminal: TerminalAlert::Bell,
            when: NotifyWhen::Always,
            events: twr_core::notify::NotifyEvent::DEFAULTS.to_vec(),
        };
        let mut n = Notifier::new(&cfg, crate::notify::Multiplexer::None);
        let tab = |name: &str, waiting| Tab {
            name: name.into(),
            hub: TabHub::Running {
                threads: 1,
                running: 0,
                permissions: waiting,
                questions: 0,
            },
        };
        let t0 = Instant::now();
        let at = |s: u64| t0 + std::time::Duration::from_secs(s);
        let mut seen = HashMap::new();
        let first = [tab("a", 0), tab("b", 0), tab("c", 0)];
        assert!(waiting_alerts(&mut seen, &first, Some("c"), &mut n, at(0)).is_empty());
        let rise = [tab("a", 1), tab("b", 1), tab("c", 5)];
        assert_eq!(
            waiting_alerts(&mut seen, &rise, Some("c"), &mut n, at(1)).len(),
            1
        );
        assert_eq!(
            waiting_alerts(&mut seen, &rise, Some("c"), &mut n, at(2)).len(),
            0
        );
        assert_eq!(
            waiting_alerts(&mut seen, &rise, Some("c"), &mut n, at(7)).len(),
            1
        );
        // Nothing more for the same counts.
        assert!(waiting_alerts(&mut seen, &rise, Some("c"), &mut n, at(20)).is_empty());
    }

    /// Alerts follow the kinds the user is notified of: with only permissions, a question in
    /// another workspace says nothing, and the reverse.
    #[test]
    fn workspace_alerts_follow_the_notified_kinds() {
        use twr_core::config::{NotifyWhen, TerminalAlert, TuiNotifyConfig};
        use twr_core::notify::NotifyEvent;
        let tab = |permissions, questions| Tab {
            name: "a".into(),
            hub: TabHub::Running {
                threads: 1,
                running: 0,
                permissions,
                questions,
            },
        };
        for (events, quiet, loud) in [
            (vec![NotifyEvent::Permission], tab(0, 1), tab(1, 0)),
            (vec![NotifyEvent::Question], tab(1, 0), tab(0, 1)),
        ] {
            let cfg = TuiNotifyConfig {
                terminal: TerminalAlert::Bell,
                when: NotifyWhen::Always,
                events,
            };
            let mut n = Notifier::new(&cfg, crate::notify::Multiplexer::None);
            let t0 = Instant::now();
            let mut seen = HashMap::new();
            assert!(waiting_alerts(&mut seen, &[tab(0, 0)], None, &mut n, t0).is_empty());
            assert!(waiting_alerts(&mut seen, &[quiet], None, &mut n, t0).is_empty());
            assert_eq!(
                waiting_alerts(&mut seen, &[loud], None, &mut n, t0).len(),
                1
            );
        }
    }

    /// Editing a line Up recalled keeps the edited line, not the draft from before.
    #[test]
    fn an_edited_history_line_is_kept() {
        let mut st = in_workspaces("default", &["default", "a"]);
        st.load_history(vec!["an old line".into()]);
        st.view.input.set("my draft");
        assert!(st.history_prev());
        let _ = st.handle_key(key(KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(st.view.input.as_str(), "an old line!");
        let saved = st.take_view();
        let mut back = in_workspaces("default", &["default", "a"]);
        back.restore_view(saved);
        assert_eq!(back.view.input.as_str(), "an old line!");
    }

    /// The decision table's selection comes back on the same row: the replayed decisions do
    /// not move it.
    #[test]
    fn the_decision_selection_survives_the_replay() {
        use crate::app::tests::{control, decision};
        use twr_core::event::{DecisionProvider, DecisionSite};
        use twr_core::testkit::ThreadScript;
        let mut t = ThreadScript::new("t-a");
        let mut evs = vec![t.created("a")];
        for i in 0..10 {
            let chosen = format!("c{i}");
            evs.push(decision(
                &mut t,
                Some(1),
                DecisionSite::Route,
                DecisionProvider::Code,
                &["x"],
                &chosen,
                0.6,
                0,
            ));
        }
        let mut st = AppState::new();
        control(&mut st, &evs);
        st.view.screen = Screen::Decisions;
        st.view.decision_sel = 3;
        let selected = st.decision_rows()[3].id.clone();
        let saved = st.take_view();
        let mut back = AppState::new();
        back.restore_view(saved.clone());
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        assert_eq!(back.decision_selected(), 3);
        // A decision came while away: the same decision stays selected, a row further down.
        evs.push(decision(
            &mut t,
            Some(1),
            DecisionSite::Route,
            DecisionProvider::Code,
            &["x"],
            "new",
            0.6,
            0,
        ));
        let mut back = AppState::new();
        back.restore_view(saved);
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        assert_eq!(back.decision_rows()[back.decision_selected()].id, selected);
        assert_eq!(back.decision_selected(), 4);
    }

    /// A kept transcript comes back with its entries, and the scroll on them; a workspace made
    /// again meanwhile (its log empty) gets none of the old one's.
    #[test]
    fn kept_transcripts_come_back_only_for_threads_still_there() {
        use crate::app::tests::{control, orch_events, thread_item};
        use twr_core::testkit::ThreadScript;
        let mut o = ThreadScript::new("t-orch");
        o.at(40);
        let evs = orch_events(&mut o, "t-orch");
        let mut st = AppState::new();
        control(&mut st, &evs);
        for i in 0..30 {
            st.apply_event(&thread_item(&o.text(&format!("reply {i}"))));
        }
        let entries = st.transcript.as_ref().unwrap().entries.len();
        assert!(entries > 0);
        st.view.scroll = 5;
        let saved = st.take_view();

        let mut back = AppState::new();
        back.restore_view(saved.clone());
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        let tr = back.transcript.as_ref().unwrap();
        assert_eq!(tr.thread, "t-orch");
        assert_eq!(tr.entries.len(), entries, "its entries, not a fresh one");
        assert_eq!(back.view.scroll, 5);

        // Made again: nothing in its log.
        let mut fresh = AppState::new();
        fresh.restore_view(saved);
        fresh.begin_catch_up(0);
        assert!(fresh.transcript.is_none());
        assert!(fresh.transcripts.is_empty());
    }

    /// A pin of a thread deleted and restored before it was pinned survives the replay of
    /// the deletion.
    #[test]
    fn pins_survive_a_replayed_deletion() {
        use crate::app::tests::control;
        use twr_core::event::{EventBody, ThreadDeleted, ThreadRestored};
        use twr_core::testkit::ThreadScript;
        let mut t = ThreadScript::new("t-a");
        let evs = vec![
            t.created("a"),
            t.event(EventBody::ThreadDeleted(ThreadDeleted::default())),
            t.event(EventBody::ThreadRestored(ThreadRestored::default())),
        ];
        let mut st = AppState::new();
        control(&mut st, &evs);
        assert!(st.registry.get("t-a").is_some());
        st.view.pins.insert("t-a".into());
        let saved = st.take_view();
        let mut back = AppState::new();
        back.restore_view(saved);
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        assert!(back.view.pins.contains("t-a"), "{:?}", back.view.pins);
    }

    /// With the help open, a click on a tab switches (the help closes too).
    #[test]
    fn a_tab_click_over_the_open_help_switches() {
        use crate::select::PaneId;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut st = in_workspaces("default", &["default", "a"]);
        st.view.help = true;
        let screen = crate::app::tests::draw(&mut st, 100, 30);
        let tab = st.view.panes.get(&PaneId::Tab("a".into())).unwrap().area;
        let at = |kind| MouseEvent {
            kind,
            column: tab.x + 1,
            row: tab.y,
            modifiers: KeyModifiers::NONE,
        };
        let left = MouseButton::Left;
        assert!(st
            .handle_mouse(at(MouseEventKind::Down(left)), &screen)
            .is_empty());
        assert!(!st.view.help);
        assert_eq!(
            st.handle_mouse(at(MouseEventKind::Up(left)), &screen),
            switch("a", false)
        );
    }

    /// A card listed only because it is pinned (Focus mode leaves idle threads out) stays
    /// selected across a switch: the pins come back before the selection is checked.
    #[test]
    fn a_pinned_card_stays_selected() {
        use crate::app::tests::control;
        use twr_core::event::TurnResult;
        use twr_core::testkit::ThreadScript;
        let (mut a, mut b) = (ThreadScript::new("t-a"), ThreadScript::new("t-b"));
        let evs = vec![
            a.created("a"),
            a.turn_started("x"),
            a.turn_ended(TurnResult::Completed),
            b.created("b"),
            b.turn_started("y"),
        ];
        let mut st = AppState::new();
        control(&mut st, &evs);
        st.view.focus_mode = true;
        st.view.pins.insert("t-a".into());
        st.set_focus(Some("t-a".into()));
        st.ensure_focus();
        assert_eq!(st.view.focus.as_deref(), Some("t-a"));
        let saved = st.take_view();
        let mut back = AppState::new();
        back.restore_view(saved);
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        assert_eq!(back.view.focus.as_deref(), Some("t-a"));
    }

    /// The request selected in the conversation comes back while it is open.
    #[test]
    fn a_selected_request_comes_back_while_open() {
        use crate::app::tests::control;
        use twr_core::event::AttentionReason;
        use twr_core::testkit::ThreadScript;
        let mut t = ThreadScript::new("t-a");
        let evs = vec![
            t.created("a"),
            t.turn_started("x"),
            t.raise("p1", AttentionReason::Permission),
        ];
        let mut st = AppState::new();
        control(&mut st, &evs);
        let id = st.attention.ordered(&HashSet::new())[0].attn_id.clone();
        st.view.request = Some(id.clone());
        let saved = st.take_view();
        let mut back = AppState::new();
        back.restore_view(saved.clone());
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        assert_eq!(back.view.request.as_deref(), Some(id.as_str()));
        // Answered meanwhile: none.
        let mut gone = AppState::new();
        gone.restore_view(saved);
        gone.begin_catch_up(2);
        control(&mut gone, &evs[..2]);
        assert_eq!(gone.view.request, None);
    }

    /// The trash overlay's commands run in the view's workspace.
    #[test]
    fn the_trash_overlay_names_the_workspace() {
        let mut st = in_workspaces("a", &["default", "a"]);
        st.attach_via = attach_via(None, Some("a"), None);
        st.apply_result(
            "trash.list",
            &serde_json::json!({
                "items": [],
                "message": "`twr trash restore <item>` brings a thread back; `twr trash empty --all` frees all of it."
            }),
        );
        let message = st.view.trash.as_ref().unwrap()["message"].as_str().unwrap();
        assert_eq!(
            message,
            "`twr -w a trash restore <item>` brings a thread back; `twr -w a trash empty --all` \
             frees all of it."
        );
    }

    /// A confirm card's detail view (its scroll and what was seen) comes back while its item
    /// is open at the same revision; not once it is answered, nor raised again.
    #[test]
    fn a_confirm_cards_scroll_comes_back_while_open() {
        use crate::app::tests::control;
        use twr_core::event::AttentionReason;
        use twr_core::testkit::ThreadScript;
        let mut t = ThreadScript::new("t-a");
        let evs = vec![
            t.created("a"),
            t.turn_started("x"),
            t.raise("p1", AttentionReason::Permission),
        ];
        let mut st = AppState::new();
        control(&mut st, &evs);
        let item = st.attention.ordered(&HashSet::new())[0].clone();
        let view = crate::app::CardView {
            attn_id: item.attn_id.clone(),
            revision: item.revision,
            width: 80,
            scroll: 3,
            seen: vec![true, true, true, true, false],
            complete: true,
        };
        st.view.card_view = view.clone();
        let saved = st.take_view();
        let replay = |evs: &[twr_core::Event]| {
            let mut back = AppState::new();
            back.restore_view(saved.clone());
            back.begin_catch_up(evs.len() as u64);
            control(&mut back, evs);
            back.view.card_view
        };
        assert_eq!(replay(&evs), view);
        let mut answered = evs.clone();
        answered.push(t.resolve("p1"));
        assert_eq!(replay(&answered), crate::app::CardView::default());
        let mut again = evs.clone();
        again.push(t.raise("p1", AttentionReason::Permission));
        assert_eq!(replay(&again), crate::app::CardView::default());
    }

    /// A hub card's text quoting a command (a delete card's trash commands) shows it run in
    /// the view's workspace; a runtime's request shows the command it asks to run as it is
    /// (approving runs that one). The items stay as raised.
    #[test]
    fn hub_cards_show_commands_in_the_workspace_requests_as_asked() {
        use crate::app::tests::control;
        use twr_core::event::{AttentionReason, EventBody, HUB_THREAD};
        use twr_core::testkit::ThreadScript;
        let mut h = ThreadScript::new(HUB_THREAD);
        let mut card = h.raise("d1", AttentionReason::Question);
        if let EventBody::AttentionRaised(p) = &mut card.body {
            p.summary = "Delete a? `twr trash restore` brings it back".into();
            p.options = vec!["Delete".into(), "Cancel".into()];
            p.ext = serde_json::json!({"twr": {"delete": {
                "threads": [{"thread": "t-a", "name": "a"}], "asked_by": "user"}}})
            .as_object()
            .cloned();
        }
        let mut t = ThreadScript::new("t-a");
        t.at(10);
        let mut perm = t.raise("p1", AttentionReason::Permission);
        if let EventBody::AttentionRaised(p) = &mut perm.body {
            p.summary = "Bash: echo `twr thread list`".into();
        }
        let evs = vec![t.created("a"), t.turn_started("x"), perm, card];
        let mut st = in_workspaces("a", &["default", "a"]);
        control(&mut st, &evs);
        st.attach_via = attach_via(None, Some("a"), None);
        let screen = crate::app::tests::draw(&mut st, 160, 40);
        let text: String = (0..40)
            .map(|y| {
                (0..160)
                    .map(|x| screen[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("`twr -w a trash restore`"), "{text}");
        assert!(text.contains("echo `twr thread list`"), "{text}");
        assert!(!text.contains("twr -w a thread list"), "{text}");
        let hub = st.attention.get("d1").unwrap();
        assert_eq!(hub.summary, "Delete a? `twr trash restore` brings it back");
    }

    /// The conversation selected (`←`) stays selected across a switch, with a request of
    /// another card than the one to return to.
    #[test]
    fn the_selected_conversation_comes_back() {
        use crate::app::tests::control;
        use twr_core::event::AttentionReason;
        use twr_core::testkit::ThreadScript;
        let (mut a, mut b) = (ThreadScript::new("t-a"), ThreadScript::new("t-b"));
        b.at(10);
        let evs = vec![
            a.created("a"),
            a.turn_started("x"),
            b.created("b"),
            b.turn_started("y"),
            b.raise("p1", AttentionReason::Permission),
        ];
        let mut st = AppState::new();
        control(&mut st, &evs);
        st.set_focus(Some("t-a".into()));
        st.view.main_focus = true;
        let id = st.attention.get("p1").map(|i| i.attn_id.clone()).unwrap();
        st.view.request = Some(id.clone());
        assert!(st.main_focused());
        assert_eq!(
            st.selected_request().map(|i| i.attn_id.as_str()),
            Some(id.as_str())
        );
        let saved = st.take_view();
        let mut back = AppState::new();
        back.restore_view(saved);
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        assert!(back.main_focused());
        assert_eq!(back.view.focus.as_deref(), Some("t-a"));
        assert_eq!(
            back.selected_request().map(|i| i.attn_id.as_str()),
            Some(id.as_str())
        );
    }

    /// The detail view of an item the action popup had no room for, and
    /// its card selected though the hidden sidebar draws none, come back after a switch away
    /// and back, while the item is still open.
    #[test]
    fn a_detail_view_comes_back_with_its_card() {
        use crate::app::tests::{control, drawn};
        use twr_core::event::{AttentionReason, EventBody};
        use twr_core::testkit::ThreadScript;
        let mut d = ThreadScript::new("t-delta");
        let mut rv = d.raise("rv9", AttentionReason::Review);
        if let EventBody::AttentionRaised(p) = &mut rv.body {
            p.summary = (0..80)
                .map(|i| format!("finding {i:02}: a long review line"))
                .collect::<Vec<_>>()
                .join("\n");
            p.options = vec!["retry".into(), "dismiss".into()];
        }
        let evs = vec![d.created("delta"), d.turn_started("review"), rv];
        let mut st = AppState::new();
        control(&mut st, &evs);
        st.set_focus(Some("t-delta".into()));
        st.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        drawn(&mut st);
        st.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        // The sidebar hidden: the detail keeps its card selected.
        st.handle_key(key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(st.view.sidebar_hidden);
        assert_eq!(st.detail_card(), Some("t-delta"));
        let saved = st.take_view();
        let mut back = AppState::new();
        back.restore_view(saved);
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);

        assert_eq!(back.focused(), Some("t-delta"));
        assert_eq!(back.detail_card(), Some("t-delta"));
    }

    /// In a dive with another card selected (Shift-→ returns to it): the dive comes back, and so
    /// does the card selected, not the dive's own.
    #[test]
    fn a_dive_keeps_the_card_selected_beside_it() {
        use crate::app::tests::control;
        use twr_core::testkit::ThreadScript;
        let (mut a, mut b) = (ThreadScript::new("t-a"), ThreadScript::new("t-b"));
        b.at(10);
        let evs = vec![
            a.created("a"),
            a.turn_started("x"),
            b.created("b"),
            b.turn_started("y"),
        ];
        let mut st = AppState::new();
        control(&mut st, &evs);
        st.dive("t-a");
        st.set_focus(Some("t-b".into()));
        st.view.main_focus = true;
        let saved = st.take_view();
        let mut back = AppState::new();
        back.restore_view(saved);
        back.begin_catch_up(evs.len() as u64);
        control(&mut back, &evs);
        assert_eq!(back.view.dive.as_deref(), Some("t-a"));
        assert_eq!(back.view.focus.as_deref(), Some("t-b"));
        assert!(back.view.main_focus);
    }
}
