//! What the TUI tells the user once, and remembers having told across runs ([`Seen`], in
//! `<root data>/tui-state.json`, [`state_file`]):
//!
//! - **Suggestions are on** ([`suggest_notice`]): the first time a next-message suggestion is
//!   due, the status bar says what it sends, to whom, what it costs and how to turn it off,
//!   and no suggestion is asked for before a frame showed that ([`crate::suggest`]).
//! - **Suggestions can't run here** (the hub has no Claude Code CLI, or one not signed in):
//!   they stop for the session without a word, but for one notice, ever
//!   ([`suggest_unavailable_notice`]).
//! - **Summaries are on** ([`summary_notice`]): the first time a summary of a thread's routine
//!   updates is due, the status bar says what it sends, to whom and how to turn them off, and
//!   none is asked for before a frame showed that ([`crate::summary`]).
//! - **Summaries can't run here**: as for suggestions, one notice ever
//!   ([`summary_unavailable_notice`]).
//! - **Shift-Enter** ([`terminal_offer`]): when the terminal can't tell Shift-Enter from Enter
//!   (no kitty keyboard protocol, and no tmux asked to pass it on), the TUI offers to set the
//!   terminal up as `twr terminal-setup` does: a yes/no question in the status bar
//!   ([`TerminalOffer::Edit`]), only for a terminal on this machine with a settings file twr
//!   edits and something to add to it. Answered, it is never asked again for that place; left
//!   unanswered, it comes back at most [`MAX_OFFERS`] starts. Anywhere twr can't edit (over
//!   SSH or mosh, inside tmux or screen, a terminal it can't tell or edit), a notice says how
//!   instead, once ([`TerminalOffer::Notice`]). Nothing is edited without the user's `y`.
//!
//! It also keeps what the user set in the TUI that is no setting of the config file: whether
//! the workspace sidebar shows (Ctrl-T, [`crate::sidebar`]), written only when it is changed
//! ([`set_workspaces_sidebar`]), so another TUI's older idea of it never overwrites it.
//!
//! The two notices that must be drawn whole before what they tell of happens have a short
//! form ([`suggest_notice_short`], [`summary_notice_short`]) that the status bar draws when it
//! has no room for the whole one (a compact terminal); either drawn whole tells it. While the
//! status bar shows one, told or not, its notes give way to it, and the bar grows for its
//! short form past its third of the screen, up to two thirds (`crate::ui::status_height`).
//!
//! A notice counts as told once a frame has shown it ([`crate::app::AppState::note_drawn`]);
//! the file only ever gains what was told (several TUIs merge into it, [`save`]). Without a
//! file to keep it in, the suggestions notice shows every run and the Shift-Enter offer never.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::keys::Keys;
use crate::terminal_setup::{Outcome, Place, Prepared, Terminal};

/// What the TUI has told the user, across runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// The suggestions notice ([`suggest_notice`]) was shown.
    pub suggest_notice: bool,
    /// The notice that suggestions can't run here was shown.
    pub suggest_unavailable: bool,
    /// The summaries notice ([`summary_notice`]) was shown.
    pub summary_notice: bool,
    /// The notice that summaries can't run here was shown.
    pub summary_unavailable: bool,
    /// The Shift-Enter offer's answer (`yes`, `no`) or `shown` for its notice, by place
    /// ([`place_key`]).
    pub terminal: BTreeMap<String, String>,
    /// How many times the Shift-Enter offer was made, by place.
    pub terminal_offers: BTreeMap<String, u32>,
    /// Whether the workspace sidebar shows, as the user last set it (`None`: never set; it
    /// shows).
    pub workspaces_sidebar: Option<bool>,
}

/// One thing told once ([`crate::app::AppState::show_once`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Once {
    /// [`suggest_notice`].
    SuggestNotice,
    /// [`suggest_unavailable_notice`].
    SuggestUnavailable,
    /// [`summary_notice`].
    SummaryNotice,
    /// [`summary_unavailable_notice`].
    SummaryUnavailable,
    /// A Shift-Enter notice ([`TerminalOffer::Notice`]) for this place.
    Terminal(String),
}

impl Seen {
    /// The state as the file holds it; anything it doesn't know is left out.
    pub fn from_json(v: &Value) -> Seen {
        let map = |k: &str| -> BTreeMap<String, Value> {
            v[k].as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default()
        };
        Seen {
            suggest_notice: v["suggest_notice"] == true,
            suggest_unavailable: v["suggest_unavailable"] == true,
            summary_notice: v["summary_notice"] == true,
            summary_unavailable: v["summary_unavailable"] == true,
            terminal: map("terminal")
                .into_iter()
                .filter_map(|(k, v)| Some((k, v.as_str()?.to_string())))
                .collect(),
            terminal_offers: map("terminal_offers")
                .into_iter()
                .filter_map(|(k, v)| Some((k, u32::try_from(v.as_u64()?).ok()?)))
                .collect(),
            workspaces_sidebar: v["prefs"]["workspaces_sidebar"].as_bool(),
        }
    }

    /// The state as the file keeps it.
    pub fn to_json(&self) -> Value {
        json!({
            "suggest_notice": self.suggest_notice,
            "suggest_unavailable": self.suggest_unavailable,
            "summary_notice": self.summary_notice,
            "summary_unavailable": self.summary_unavailable,
            "terminal": self.terminal,
            "terminal_offers": self.terminal_offers,
            "prefs": { "workspaces_sidebar": self.workspaces_sidebar },
        })
    }

    /// Adds what `other` told: a flag set in either stays set, an answer once given stays
    /// (the first one), the larger count of offers. A preference stays as this one has it
    /// (the file's, as [`save`] merges): only [`set_workspaces_sidebar`] changes it.
    pub fn merge(&mut self, other: &Seen) {
        self.workspaces_sidebar = self.workspaces_sidebar.or(other.workspaces_sidebar);
        self.suggest_notice |= other.suggest_notice;
        self.suggest_unavailable |= other.suggest_unavailable;
        self.summary_notice |= other.summary_notice;
        self.summary_unavailable |= other.summary_unavailable;
        for (k, v) in &other.terminal {
            self.terminal.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, n) in &other.terminal_offers {
            let mine = self.terminal_offers.entry(k.clone()).or_default();
            *mine = (*mine).max(*n);
        }
    }

    /// Records `what` as told.
    pub fn told(&mut self, what: &Once) {
        match what {
            Once::SuggestNotice => self.suggest_notice = true,
            Once::SuggestUnavailable => self.suggest_unavailable = true,
            Once::SummaryNotice => self.summary_notice = true,
            Once::SummaryUnavailable => self.summary_unavailable = true,
            Once::Terminal(key) => {
                self.terminal
                    .entry(key.clone())
                    .or_insert_with(|| "shown".into());
            }
        }
    }
}

/// Where the state is kept: `tui-state.json` in the data directory of the root the
/// environment names (shared by its workspaces: it is about the user and the terminal, not a
/// hub). `None` when the environment names none.
pub fn state_file() -> Option<PathBuf> {
    let paths = twr_core::paths::Paths::from_env().ok()?;
    Some(paths.root().data_dir.join("tui-state.json"))
}

/// The state in the file at `path`; nothing told when it can't be read.
pub fn load(path: &Path) -> Seen {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|v| Seen::from_json(&v))
        .unwrap_or_default()
}

/// Adds `seen` to the file at `path` ([`update`]): what another TUI wrote meanwhile stays
/// ([`Seen::merge`]).
pub fn save(path: &Path, seen: &Seen) -> std::io::Result<()> {
    update(path, |all| all.merge(seen))
}

/// Records in the file at `path` that the workspace sidebar shows (`on`) or not, whatever it
/// said before.
pub fn set_workspaces_sidebar(path: &Path, on: bool) -> std::io::Result<()> {
    update(path, |all| all.workspaces_sidebar = Some(on))
}

/// Takes one of the Shift-Enter offers left for place `key` ([`MAX_OFFERS`]), in the file at
/// `path` under its lock, so that TUIs starting together make no more between them: whether
/// one was left (none once the place was answered or told), and the state as the file now
/// holds it.
pub fn reserve_offer(path: &Path, key: &str) -> std::io::Result<(bool, Seen)> {
    let mut got = false;
    let mut now = Seen::default();
    update(path, |all| {
        let made = all.terminal_offers.entry(key.to_string()).or_default();
        if !all.terminal.contains_key(key) && *made < MAX_OFFERS {
            *made += 1;
            got = true;
        }
        now = all.clone();
    })?;
    Ok((got, now))
}

/// Changes the state in the file at `path` with `change` (the file and its folder made
/// private), under an exclusive lock on `<path>.lock`; the new file replaces the old one whole.
pub fn update(path: &Path, change: impl FnOnce(&mut Seen)) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        twr_core::paths::ensure_private_dir(dir).map_err(std::io::Error::other)?;
    }
    let lock = twr_core::paths::open_private_append(&path.with_extension("lock"))?;
    lock.lock()?;
    let mut all = load(path);
    change(&mut all);
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = twr_core::paths::open_private_append(&tmp)?;
        f.set_len(0)?;
        writeln!(f, "{}", all.to_json())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The status bar's notice the first time a suggestion is due ([`Once::SuggestNotice`]),
/// naming the models `models` names.
pub fn suggest_notice(models: &twr_core::config::SuggestModels) -> String {
    format!(
        "suggestions are on: after a turn of the thread you see ends, its prompt (2 KB) and the \
         end of its reply (6 KB) go to a small model with your own login, {} through Claude Code \
         (to Anthropic) or {} through Codex (to OpenAI), the thread's own runtime first: about \
         $0.0002 each at list price, counting toward your plan's usage · Tab or → takes one · \
         /suggest off, or [tui] suggest = false, turns them off",
        models.claude_code, models.codex
    )
}

/// [`suggest_notice`] said short, for a status bar with no room for it whole (a compact
/// terminal): still what is sent, to whom, what it costs and how to turn them off.
pub fn suggest_notice_short(models: &twr_core::config::SuggestModels) -> String {
    format!(
        "suggestions on: a turn's prompt and reply end (8 KB) go to {} (Claude Code, \
         Anthropic) or {} (Codex, OpenAI) with your login, ~$0.0002 each · /suggest off",
        models.claude_code, models.codex
    )
}

/// The one notice that suggestions can't run here, `why` being the hub's reason.
pub fn suggest_unavailable_notice(why: &str) -> String {
    format!(
        "suggestions are off: {why} on the hub's machine, so nothing is sent · they work once \
         Claude Code is installed and signed in (/suggest on); [tui] suggest = false keeps \
         them off"
    )
}

/// The status bar's notice the first time a summary is due ([`Once::SummaryNotice`]),
/// naming the models `models` names and the pace `every` (`[tui] summary_every`).
pub fn summary_notice(
    models: &twr_core::config::SuggestModels,
    every: std::time::Duration,
) -> String {
    format!(
        "thread summaries are on: for a thread whose routine updates fold into one row, its \
         name, state, what it is doing (300 B) and those updates (3 KB), never an item or a \
         message of yours, go to a small model with your own login, {} through Claude Code (to \
         Anthropic) or {} through Codex (to OpenAI), the thread's own runtime first, at most \
         once per {} a thread: about $0.0002 each at list price, counting toward your plan's \
         usage · /summaries off, or [tui] summaries = false, turns them off",
        models.claude_code,
        models.codex,
        crate::summary::every_words(every)
    )
}

/// [`summary_notice`] said short, for a status bar with no room for it whole (a compact
/// terminal): still what is sent, to whom, how often, what it costs and how to turn them off.
pub fn summary_notice_short(
    models: &twr_core::config::SuggestModels,
    every: std::time::Duration,
) -> String {
    format!(
        "summaries on: a thread's name, state, activity and routine updates (3 KB), no items, \
         go to {} (Claude Code, Anthropic) or {} (Codex, OpenAI) with your login, ≤1 per {} a \
         thread, ~$0.0002 each · /summaries off",
        models.claude_code,
        models.codex,
        crate::summary::every_words(every)
    )
}

/// The one notice that summaries can't run here, `why` being the hub's reason.
pub fn summary_unavailable_notice(why: &str) -> String {
    format!(
        "thread summaries are off: {why}, so nothing is sent and routine updates fold into \
         rows of their own words · they work once Claude Code or Codex is installed and signed \
         in on the hub's machine (/summaries on); [tui] summaries = false keeps them off"
    )
}

/// The place key of the notice that tmux sends Shift-Enter in a form twr can't read
/// ([`crate::keys::TMUX_NOTICE`]), told once like the others.
pub const TMUX_FORMAT_KEY: &str = "tmux-extended-keys-format";

/// Unanswered Shift-Enter offers made for a place before the TUI stops making them.
pub const MAX_OFFERS: u32 = 3;

/// What the TUI offers about Shift-Enter at startup ([`terminal_offer`]).
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalOffer {
    /// A yes/no question: twr adds the binding to `file`, the settings of `terminal`.
    Edit {
        key: String,
        place: Place,
        terminal: Terminal,
        file: PathBuf,
    },
    /// What to do, said once.
    Notice { key: String, text: String },
}

impl TerminalOffer {
    /// The place it is for ([`place_key`]).
    pub fn key(&self) -> &str {
        match self {
            TerminalOffer::Edit { key, .. } | TerminalOffer::Notice { key, .. } => key,
        }
    }

    /// The question the status bar asks for [`TerminalOffer::Edit`].
    pub fn question(&self) -> Option<String> {
        let TerminalOffer::Edit {
            terminal,
            file,
            place,
            ..
        } = self
        else {
            return None;
        };
        Some(format!(
            "Shift-Enter sends Enter in {}. Make it send Alt-Enter, a new line in twr, as \
             Claude Code's /terminal-setup does? twr adds a key binding to {} (backed up \
             first) · y: yes · n: don't ask again",
            terminal.title(),
            tilde(file, &place.home)
        ))
    }
}

/// `path` with the home directory `home` written `~`.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() && !rest.as_os_str().is_empty() => {
            format!("~/{}", rest.display())
        }
        _ => path.display().to_string(),
    }
}

/// The place an answer is remembered for: over `mosh` or `ssh`, inside `tmux` or `screen`,
/// else `local:<terminal>` (`local:unknown` when the environment names none).
pub fn place_key(place: &Place) -> String {
    if let Some(r) = place.remote {
        return r.to_string();
    }
    if let Some(m) = place.multiplexer {
        return m.to_string();
    }
    format!("local:{}", place.terminal.map_or("unknown", Terminal::name))
}

/// What to offer about Shift-Enter at startup, if anything: nothing when Shift-Enter may
/// arrive apart from Enter (`keys`), for an agent's TUI (`agent`), or once the place was
/// answered or told (`seen`). Over SSH or mosh, inside tmux or screen, or in a terminal twr
/// can't tell or edit: a notice. A terminal twr edits: the question when `prepare` (what
/// [`crate::terminal_setup::prepare`] finds) has something to write, at most [`MAX_OFFERS`]
/// times; nothing when the binding is there already.
pub fn terminal_offer(
    keys: &Keys,
    place: &Place,
    agent: bool,
    seen: &Seen,
    prepare: impl Fn(&Place, Terminal) -> anyhow::Result<Prepared>,
) -> Option<TerminalOffer> {
    // tmux asked to pass Shift-Enter on may do so, but not over mosh, which never passes the
    // terminal's extended keys on to tmux.
    let tmux_may = keys.tmux_asked && place.remote != Some("mosh");
    if keys.shift_enter || tmux_may || agent {
        return None;
    }
    let key = place_key(place);
    if seen.terminal.contains_key(&key) {
        return None;
    }
    let notice = |text: String| {
        Some(TerminalOffer::Notice {
            key: key.clone(),
            text,
        })
    };
    let how = "Ctrl-J always starts a new line";
    match (place.remote, place.multiplexer) {
        (Some("mosh"), _) => return notice(crate::keys::MOSH_NOTICE.to_string()),
        (Some(r), _) => {
            return notice(format!(
                "over {r}, Shift-Enter arrives as Enter: run `twr terminal-setup` on the machine \
                 your terminal runs on, to make it send Alt-Enter, a new line in twr · {how}"
            ))
        }
        (None, Some(m)) => {
            return notice(format!(
                "inside {m}, Shift-Enter arrives as Enter: run `twr terminal-setup` outside {m}, \
                 to make your terminal send Alt-Enter, a new line in twr · {how}"
            ))
        }
        (None, None) => {}
    }
    let Some(terminal) = place.terminal else {
        return notice(format!(
            "Shift-Enter arrives as Enter in this terminal: `twr terminal-setup --print` says \
             how to make it send Alt-Enter, a new line in twr · {how}"
        ));
    };
    let by_hand = || {
        notice(format!(
            "Shift-Enter arrives as Enter in {}: `twr terminal-setup` says how to make it send \
             Alt-Enter, a new line in twr · {how}",
            terminal.title()
        ))
    };
    match prepare(place, terminal) {
        Ok(Prepared::Write { file, .. }) => {
            let offers = seen.terminal_offers.get(&key).copied().unwrap_or(0);
            (offers < MAX_OFFERS).then(|| TerminalOffer::Edit {
                key: key.clone(),
                place: place.clone(),
                terminal,
                file,
            })
        }
        // Set up already: Alt-Enter arrives, a new line.
        Ok(Prepared::Done(Outcome::Already { .. })) => None,
        Ok(Prepared::Done(_)) | Err(_) => by_hand(),
    }
}

/// The status line for what the edit of an accepted offer did (`terminal`'s settings, under
/// the home directory `home`).
pub fn edit_status(terminal: Terminal, outcome: &anyhow::Result<Outcome>, home: &Path) -> String {
    let tilde = |p: &Path| tilde(p, home);
    let title = terminal.title();
    match outcome {
        Ok(Outcome::Added { file, backup }) => {
            let backup = backup
                .as_ref()
                .map(|b| format!(" (the old one is {})", tilde(b)))
                .unwrap_or_default();
            let when = match terminal {
                Terminal::VsCode(_) | Terminal::Zed => "in new terminals",
                _ => "once its config is reloaded or it restarts",
            };
            format!(
                "{title}: Shift-Enter now sends Alt-Enter, set in {}{backup}; it works {when}",
                tilde(file)
            )
        }
        Ok(Outcome::Already { .. }) => format!("{title}: Shift-Enter was set up already"),
        Ok(Outcome::Other { file }) => format!(
            "{title}: Shift-Enter has another binding in {}, left as it is (`twr terminal-setup` \
             says how to change it)",
            tilde(file)
        ),
        Ok(Outcome::Unsafe { file, why }) => format!(
            "{title}: {why} ({}), so twr left it alone: `twr terminal-setup` says what to add",
            tilde(file)
        ),
        Ok(Outcome::Manual) => {
            format!("{title}: `twr terminal-setup` says how to set Shift-Enter up")
        }
        Err(e) => format!("{title}: Shift-Enter not set up: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(terminal: Option<Terminal>) -> Place {
        Place {
            terminal,
            home: PathBuf::from("/home/me"),
            ..Default::default()
        }
    }

    fn write(file: &str) -> impl Fn(&Place, Terminal) -> anyhow::Result<Prepared> + '_ {
        move |_, _| {
            Ok(Prepared::Write {
                file: PathBuf::from(file),
                old: None,
                new: "x".into(),
            })
        }
    }

    /// The state round-trips through its JSON, ignores what it doesn't know, and merges as a
    /// union: what either told stays told.
    #[test]
    fn the_state_keeps_what_was_told() {
        let mut a = Seen::default();
        a.told(&Once::SuggestNotice);
        a.told(&Once::SummaryNotice);
        a.told(&Once::Terminal("mosh".into()));
        a.terminal.insert("local:alacritty".into(), "no".into());
        a.terminal_offers.insert("local:alacritty".into(), 2);
        assert_eq!(Seen::from_json(&a.to_json()), a);
        assert_eq!(Seen::from_json(&json!("garbage")), Seen::default());
        let odd = json!({"suggest_notice": "yes", "terminal": {"ssh": 3}, "later": true,
                         "terminal_offers": {"x": -1, "y": 2}});
        let got = Seen::from_json(&odd);
        assert!(!got.suggest_notice && got.terminal.is_empty());
        assert_eq!(got.terminal_offers, BTreeMap::from([("y".into(), 2)]));
        let mut b = Seen::default();
        b.told(&Once::SuggestUnavailable);
        b.told(&Once::SummaryUnavailable);
        b.terminal.insert("local:alacritty".into(), "yes".into());
        b.terminal.insert("ssh".into(), "shown".into());
        b.terminal_offers.insert("local:alacritty".into(), 3);
        b.merge(&a);
        assert!(b.suggest_notice && b.suggest_unavailable);
        assert!(b.summary_notice && b.summary_unavailable);
        assert_eq!(b.terminal["local:alacritty"], "yes");
        assert_eq!(b.terminal["mosh"], "shown");
        assert_eq!(b.terminal_offers["local:alacritty"], 3);
    }

    /// Offers are taken under the file's lock: TUIs starting together make no more than
    /// [`MAX_OFFERS`] between them, and none once the place was answered.
    #[test]
    fn offers_are_reserved_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui-state.json");
        let got: Vec<bool> = std::thread::scope(|s| {
            let runs: Vec<_> = (0..8)
                .map(|_| s.spawn(|| reserve_offer(&path, "local:zed").unwrap().0))
                .collect();
            runs.into_iter().map(|r| r.join().unwrap()).collect()
        });
        assert_eq!(
            got.iter().filter(|g| **g).count(),
            MAX_OFFERS as usize,
            "{got:?}"
        );
        assert_eq!(load(&path).terminal_offers["local:zed"], MAX_OFFERS);
        let mut answered = Seen::default();
        answered.terminal.insert("local:vscode".into(), "no".into());
        save(&path, &answered).unwrap();
        let (got, now) = reserve_offer(&path, "local:vscode").unwrap();
        assert!(!got);
        assert_eq!(now.terminal["local:vscode"], "no");
    }

    /// Saving adds to what the file holds (another TUI's), and the file is private.
    #[test]
    fn saving_merges_into_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join("tui-state.json");
        assert_eq!(load(&path), Seen::default());
        let mut one = Seen::default();
        one.told(&Once::SuggestNotice);
        save(&path, &one).unwrap();
        let mut two = Seen::default();
        two.told(&Once::Terminal("ssh".into()));
        save(&path, &two).unwrap();
        let got = load(&path);
        assert!(got.suggest_notice);
        assert_eq!(got.terminal["ssh"], "shown");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }

    /// The offer is made only where Shift-Enter can't be told from Enter, for a local terminal
    /// twr edits that lacks the binding, until answered or made [`MAX_OFFERS`] times; elsewhere
    /// a notice says how, once; nothing for an agent's TUI.
    #[test]
    fn shift_enter_is_offered_only_where_twr_can_set_it_up() {
        let none = Keys::default();
        let seen = Seen::default();
        let alacritty = place(Some(Terminal::Alacritty));
        let file = "/home/me/.config/alacritty/alacritty.toml";
        let got = terminal_offer(&none, &alacritty, false, &seen, write(file)).unwrap();
        let TerminalOffer::Edit { key, terminal, .. } = &got else {
            panic!("{got:?}")
        };
        assert_eq!(
            (key.as_str(), *terminal),
            ("local:alacritty", Terminal::Alacritty)
        );
        let q = got.question().unwrap();
        assert!(q.contains("~/.config/alacritty/alacritty.toml"), "{q}");
        assert!(q.contains("y: yes · n: don't ask again"), "{q}");
        // Shift-Enter arrives apart (kitty protocol), or tmux was asked to pass it on: nothing.
        let works = Keys {
            shift_enter: true,
            ..Keys::default()
        };
        assert_eq!(
            terminal_offer(&works, &alacritty, false, &seen, write(file)),
            None
        );
        let asked = Keys {
            tmux_asked: true,
            ..Keys::default()
        };
        assert_eq!(
            terminal_offer(&asked, &alacritty, false, &seen, write(file)),
            None
        );
        // But not over mosh, which passes no extended key on to tmux: the mosh notice, once.
        let over_mosh = Place {
            remote: Some("mosh"),
            multiplexer: Some("tmux"),
            ..alacritty.clone()
        };
        let asked_mosh = Keys {
            tmux_asked: true,
            mosh: true,
            ..Keys::default()
        };
        let got = terminal_offer(&asked_mosh, &over_mosh, false, &seen, write(file)).unwrap();
        assert_eq!(
            got,
            TerminalOffer::Notice {
                key: "mosh".into(),
                text: crate::keys::MOSH_NOTICE.into()
            }
        );
        let mut told = Seen::default();
        told.told(&Once::Terminal("mosh".into()));
        assert_eq!(
            terminal_offer(&asked_mosh, &over_mosh, false, &told, write(file)),
            None
        );
        assert_eq!(
            terminal_offer(&none, &alacritty, true, &seen, write(file)),
            None
        );
        // Answered either way, never again; unanswered, MAX_OFFERS times.
        for answer in ["yes", "no"] {
            let mut s = Seen::default();
            s.terminal.insert("local:alacritty".into(), answer.into());
            assert_eq!(
                terminal_offer(&none, &alacritty, false, &s, write(file)),
                None
            );
        }
        let mut s = Seen::default();
        s.terminal_offers
            .insert("local:alacritty".into(), MAX_OFFERS - 1);
        assert!(terminal_offer(&none, &alacritty, false, &s, write(file)).is_some());
        s.terminal_offers
            .insert("local:alacritty".into(), MAX_OFFERS);
        assert_eq!(
            terminal_offer(&none, &alacritty, false, &s, write(file)),
            None
        );
        // The binding there already: nothing; another binding, or a file twr can't edit: how.
        let done = |o: Outcome| move |_: &Place, _| Ok(Prepared::Done(o.clone()));
        let already = Outcome::Already { file: file.into() };
        assert_eq!(
            terminal_offer(&none, &alacritty, false, &seen, done(already)),
            None
        );
        let other = Outcome::Other { file: file.into() };
        let got = terminal_offer(&none, &alacritty, false, &seen, done(other)).unwrap();
        assert!(matches!(&got, TerminalOffer::Notice { text, .. }
            if text.contains("Alacritty") && text.contains("twr terminal-setup")));
        let iterm = place(Some(Terminal::ITerm2));
        let got = terminal_offer(&none, &iterm, false, &seen, done(Outcome::Manual)).unwrap();
        assert_eq!(got.key(), "local:iterm2");
        assert!(got.question().is_none());
        let got = terminal_offer(&none, &place(None), false, &seen, write(file)).unwrap();
        assert!(matches!(&got, TerminalOffer::Notice { key, text }
            if key == "local:unknown" && text.contains("--print")));
        // Elsewhere twr edits nothing: a notice, keyed by where, never the question.
        for (remote, multiplexer, key, says) in [
            (Some("mosh"), None, "mosh", "predict-overwrite"),
            (Some("mosh"), Some("tmux"), "mosh", "predict-overwrite"),
            (
                Some("ssh"),
                None,
                "ssh",
                "on the machine your terminal runs on",
            ),
            (None, Some("tmux"), "tmux", "outside tmux"),
            (None, Some("screen"), "screen", "outside screen"),
        ] {
            let p = Place {
                remote,
                multiplexer,
                ..alacritty.clone()
            };
            let got = terminal_offer(&none, &p, false, &seen, write(file)).unwrap();
            let TerminalOffer::Notice { key: k, text } = &got else {
                panic!("{got:?}")
            };
            assert_eq!(k, key);
            assert!(text.contains(says), "{text}");
            let mut s = Seen::default();
            s.told(&Once::Terminal(key.into()));
            assert_eq!(terminal_offer(&none, &p, false, &s, write(file)), None);
        }
    }

    /// What an accepted offer's edit did, in one status line.
    #[test]
    fn the_edit_says_what_it_did() {
        let file = PathBuf::from("/x/alacritty.toml");
        let added = Ok(Outcome::Added {
            file: file.clone(),
            backup: Some(PathBuf::from("/x/alacritty.toml.twr-backup-1")),
        });
        let s = edit_status(Terminal::Alacritty, &added, Path::new("/home/me"));
        assert!(s.contains("set in /x/alacritty.toml"), "{s}");
        assert!(s.contains("twr-backup-1"), "{s}");
        assert!(s.contains("reloaded"), "{s}");
        let s = edit_status(
            Terminal::VsCode(crate::terminal_setup::Editor::Code),
            &Ok(Outcome::Added {
                file: file.clone(),
                backup: None,
            }),
            Path::new("/home/me"),
        );
        assert!(
            s.contains("in new terminals") && !s.contains("old one"),
            "{s}"
        );
        let s = edit_status(
            Terminal::Alacritty,
            &Err(anyhow::anyhow!("disk full")),
            Path::new("/home/me"),
        );
        assert!(s.contains("not set up: disk full"), "{s}");
    }
}
