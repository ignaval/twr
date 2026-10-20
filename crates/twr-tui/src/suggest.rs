//! Next-message suggestions, on unless `[tui] suggest = false` (or `/suggest off`): after a
//! turn of the main pane's thread ends while the TUI watches and nothing is typed, the TUI
//! asks the hub for a likely next message (`suggest.next`, `twr_hub::hub::suggest`), off the
//! IO loop on a connection of its own, and shows it as dim ghost text in the empty input box.
//! Tab or → puts it in the box, to send with Enter like anything typed; typing anything else
//! types over it, and Enter on the empty line does what it always does: a suggestion is never
//! sent by itself. A new turn, or another thread on the main pane, takes it away.
//!
//! The first one due waits for the notice that says what suggestions send and how to turn
//! them off ([`crate::firstrun::suggest_notice`]) to be on screen; it is asked for only then,
//! and the notice is never shown again. Where the hub can't run them (no Claude Code CLI, or
//! none signed in: `hub.info` `suggest.unavailable`, or a call's `unavailable: ...` reason),
//! they stop for the session without a call, with one notice ever
//! ([`AppState::suggest_unavailable`]).

use serde_json::Value;

use crate::app::{AppState, Screen};

/// How the hub's reason starts when suggestions can't run there at all
/// (`twr_hub::hub::suggest::UNAVAILABLE`).
pub const UNAVAILABLE: &str = "unavailable: ";

/// A suggested next message ([`AppState::suggestion`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The thread it is for.
    pub thread: String,
    /// The turn it follows.
    pub turn: u64,
    /// The message.
    pub text: String,
    /// When a frame first showed it (Tab or → takes it only [`SUGGEST_SETTLE`] after).
    pub shown: Option<std::time::Instant>,
}

/// How long a suggestion must have been on screen before Tab or → takes it: a key pressed as
/// it appeared was meant for what was there before (Tab selects a card).
pub const SUGGEST_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// A suggestion to ask for ([`AppState::take_suggest_due`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub thread: String,
    pub turn: u64,
    /// The model for each runtime that may answer (`[tui] suggest_model`): the hub picks the
    /// runtime.
    pub models: twr_core::config::SuggestModels,
}

impl AppState {
    /// Turn `turn` of `thread` ended, live: with suggestions on, the thread on the main pane
    /// and nothing typed, a suggestion is due.
    pub(crate) fn suggest_after(&mut self, thread: &str, turn: u64) {
        if self.suggestion.as_ref().is_some_and(|s| s.thread == thread) {
            self.suggestion = None;
        }
        if !self.view.suggest
            || self.main_thread().as_deref() != Some(thread)
            || !self.view.input.is_empty()
        {
            return;
        }
        self.suggest_due = Some((thread.to_string(), turn));
    }

    /// A turn of `thread` started: its suggestion goes.
    pub(crate) fn suggest_drop(&mut self, thread: &str) {
        if self.suggestion.as_ref().is_some_and(|s| s.thread == thread) {
            self.suggestion = None;
        }
        if self.suggest_due.as_ref().is_some_and(|(t, _)| t == thread) {
            self.suggest_due = None;
        }
    }

    /// The suggestion to ask for now, if one is due; it is then on its way. The first one ever
    /// waits for the notice of what suggestions send to have been on screen
    /// ([`crate::firstrun::suggest_notice`]): until then it stays due, and the notice is shown.
    pub fn take_suggest_due(&mut self) -> Option<Ask> {
        if !self.view.suggest {
            self.suggest_due = None;
            return None;
        }
        if !self.seen.suggest_notice {
            self.suggest_due.as_ref()?;
            let notice = crate::firstrun::suggest_notice(&self.view.suggest_model);
            let on_screen = self.view.status.as_deref() == Some(notice.as_str());
            match &self.once {
                // A question has the status bar: after it.
                _ if self.view.confirm.is_some() => {}
                // Shown, not drawn yet: wait for a frame.
                Some((crate::firstrun::Once::SuggestNotice, _)) if on_screen => {}
                // Shown, then replaced before a frame showed it: shown again.
                Some((crate::firstrun::Once::SuggestNotice, _)) => {
                    self.view.status = Some(notice);
                }
                // Another notice told once that a frame hasn't shown: after it, unless it was
                // replaced meanwhile (it comes back at the next start).
                Some((_, text)) if self.view.status.as_deref() == Some(text) => {}
                _ => self.show_once(crate::firstrun::Once::SuggestNotice, notice),
            }
            return None;
        }
        let (thread, turn) = self.suggest_due.take()?;
        self.suggest_asked = Some((thread.clone(), turn, self.suggest_epoch));
        Some(Ask {
            thread,
            turn,
            models: self.view.suggest_model.clone(),
        })
    }

    /// What `suggest.next` answered (`{failed, error}` when the call failed): kept while it is
    /// for the main pane's thread and nothing ran since; the reason there is none goes to the
    /// status bar only when a call failed, once.
    pub(crate) fn suggest_result(&mut self, r: &Value) {
        self.suggest_stats = r["stats"].clone();
        let (Some(thread), Some(turn)) = (r["thread"].as_str(), r["turn"].as_u64()) else {
            if let Some(e) = r["error"].as_str() {
                self.note_suggest_failure(e);
            }
            return;
        };
        if let Some(why) = r["reason"]
            .as_str()
            .and_then(|r| r.strip_prefix(UNAVAILABLE))
        {
            let why = why.to_string();
            self.suggest_unavailable(&why);
            return;
        }
        if let Some(reason) = r["reason"]
            .as_str()
            .and_then(|r| r.strip_prefix("failed: "))
        {
            let reason = reason.to_string();
            self.note_suggest_failure(&reason);
        }
        let Some(text) = r["suggestion"].as_str().filter(|t| !t.trim().is_empty()) else {
            return;
        };
        // Asked before a send of the user's since: about a conversation that went on.
        let asked = self
            .suggest_asked
            .as_ref()
            .is_some_and(|(t, n, epoch)| t == thread && *n == turn && *epoch == self.suggest_epoch);
        let still = asked
            && self.main_thread().as_deref() == Some(thread)
            && self
                .registry
                .get(thread)
                .is_some_and(|v| v.current_turn.is_none() && v.turns == turn);
        if still {
            self.suggestion = Some(crate::suggest::Suggestion {
                thread: thread.to_string(),
                turn,
                text: text.to_string(),
                shown: None,
            });
        }
    }

    /// The hub can't run suggestions (`why`: no Claude Code CLI, or none signed in): none is
    /// asked for again in this run, and the status bar says so once ever
    /// ([`crate::firstrun::Seen::suggest_unavailable`]), when nothing else is on it.
    pub(crate) fn suggest_unavailable(&mut self, why: &str) {
        if self.view.suggest {
            self.view.suggest = false;
            self.suggestion = None;
            self.suggest_due = None;
            self.suggest_off_why = Some(why.to_string());
            // The notice that suggestions are on would say what can't happen here.
            if self
                .once
                .as_ref()
                .is_some_and(|(w, _)| *w == crate::firstrun::Once::SuggestNotice)
            {
                self.once = None;
                self.view.status = None;
            }
        }
        self.tell_suggest_unavailable();
    }

    /// The notice that suggestions can't run here, once ever, when the status bar is free.
    fn tell_suggest_unavailable(&mut self) {
        let Some(why) = &self.suggest_off_why else {
            return;
        };
        let busy = self.once.is_some() || self.view.confirm.is_some() || self.view.status.is_some();
        if self.seen.suggest_unavailable || busy {
            return;
        }
        let text = crate::firstrun::suggest_unavailable_notice(why);
        self.show_once(crate::firstrun::Once::SuggestUnavailable, text);
    }

    /// What `hub.info` says of suggestions: `suggest.unavailable`, why they can't run there
    /// (not after the user turned them on in this run: a call says it then).
    pub(crate) fn apply_suggest_info(&mut self, info: &Value) {
        let why = info["suggest"]["unavailable"]
            .as_str()
            .and_then(|r| r.strip_prefix(UNAVAILABLE));
        match why {
            Some(why) if !self.suggest_turned_on => {
                let why = why.to_string();
                self.suggest_unavailable(&why);
            }
            _ => self.tell_suggest_unavailable(),
        }
    }

    /// A suggestion call failed: the status bar says so the first time.
    fn note_suggest_failure(&mut self, why: &str) {
        if !std::mem::replace(&mut self.suggest_failed, true) {
            self.view.status = Some(format!("no suggestion: {why} (/suggest off stops them)"));
        }
    }

    /// The suggestion the empty input box shows: suggestions on, nothing typed, the stack
    /// view (or the compact conversation) on its thread, no popup or overlay open, and no
    /// turn of its thread running.
    pub fn shown_suggestion(&self) -> Option<&str> {
        let s = self.suggestion.as_ref()?;
        let v = &self.view;
        let open = v.action.is_some() || v.help || v.trash.is_some() || v.confirm.is_some();
        (v.suggest
            && v.input.is_empty()
            && v.screen == Screen::Stack
            && !open
            && !self.picker_open()
            && self.main_thread().as_deref() == Some(s.thread.as_str())
            && self
                .registry
                .get(&s.thread)
                .is_some_and(|t| t.current_turn.is_none()))
        .then_some(s.text.as_str())
    }

    /// Tab or → on the empty line with a suggestion shown: it goes in the box, to edit or send
    /// with Enter. Returns whether it did.
    pub(crate) fn accept_suggestion(&mut self) -> bool {
        let Some(text) = self.shown_suggestion().map(str::to_string) else {
            return false;
        };
        // Only one read: drawn, and on screen a moment before the key was read (a key read
        // earlier and handled late was meant for what was there then).
        let key_at = self.input_at.unwrap_or_else(std::time::Instant::now);
        let read = self
            .suggestion
            .as_ref()
            .and_then(|s| s.shown)
            .is_some_and(|shown| {
                key_at
                    .checked_duration_since(shown)
                    .is_some_and(|d| d >= SUGGEST_SETTLE)
            });
        if !read {
            return false;
        }
        self.suggestion = None;
        let (cols, rows) = (self.input_cols(), self.input_rows());
        // A message, never a command: a suggestion that starts with `!` or `/` is escaped.
        let line = crate::app::literal_line(&text);
        self.view.input.set(&line);
        self.taken_suggestion = Some(line);
        self.view.input.keep_visible(cols, rows);
        self.view.popup_sel = 0;
        true
    }

    /// `/suggest [on|off]`: turns suggestions on or off for this TUI (no argument toggles),
    /// and says what that means.
    pub(crate) fn toggle_suggest(&mut self, on: Option<bool>) {
        let on = on.unwrap_or(!self.view.suggest);
        self.view.suggest = on;
        self.suggest_turned_on = on;
        self.suggest_off_why = None;
        // The notice is told only by itself, drawn whole: what `/suggest` says is not it.
        if self
            .once
            .as_ref()
            .is_some_and(|(w, _)| *w == crate::firstrun::Once::SuggestNotice)
        {
            self.once = None;
        }
        if !on {
            self.suggestion = None;
            self.suggest_due = None;
        }
        self.suggest_failed = false;
        let calls = self.suggest_stats["calls"].as_u64().unwrap_or(0);
        let m = &self.view.suggest_model;
        self.view.status = Some(if on {
            format!(
                "suggestions on: after a turn of the thread you see, its prompt and the end of \
                 its reply go to {} through Claude Code (to Anthropic) or {} through Codex (to \
                 OpenAI), the thread's own runtime first, with your login · Tab or → takes one \
                 · {calls} asked so far",
                m.claude_code, m.codex
            )
        } else {
            "suggestions off".to_string()
        });
    }
}
