//! Next-message suggestions, when `[tui] suggest = true` (or `/suggest on`): after a turn of
//! the main pane's thread ends while the TUI watches and nothing is typed, the TUI asks the
//! hub for a likely next message (`suggest.next`, `twr_hub::hub::suggest`), off the IO loop
//! on a connection of its own, and shows it as dim ghost text in the empty input box. Tab or
//! → puts it in the box, to send with Enter like anything typed; typing anything else types
//! over it, and Enter on the empty line does what it always does: a suggestion is never sent
//! by itself. A new turn, or another thread on the main pane, takes it away.

use serde_json::Value;

use crate::app::{AppState, Screen};

/// A suggested next message ([`AppState::suggestion`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The thread it is for.
    pub thread: String,
    /// The turn it follows.
    pub turn: u64,
    /// The message.
    pub text: String,
}

/// A suggestion to ask for ([`AppState::take_suggest_due`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub thread: String,
    pub turn: u64,
    pub model: String,
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

    /// The suggestion to ask for now, if one is due; it is then on its way.
    pub fn take_suggest_due(&mut self) -> Option<Ask> {
        let (thread, turn) = self.suggest_due.take()?;
        if !self.view.suggest {
            return None;
        }
        Some(Ask {
            thread,
            turn,
            model: self.view.suggest_model.clone(),
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
        let still = self.main_thread().as_deref() == Some(thread)
            && self
                .registry
                .get(thread)
                .is_some_and(|v| v.current_turn.is_none() && v.turns == turn);
        if still {
            self.suggestion = Some(crate::suggest::Suggestion {
                thread: thread.to_string(),
                turn,
                text: text.to_string(),
            });
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
        self.suggestion = None;
        let (cols, rows) = (self.input_cols(), self.input_rows());
        self.view.input.set(&text);
        self.view.input.keep_visible(cols, rows);
        self.view.popup_sel = 0;
        true
    }

    /// `/suggest [on|off]`: turns suggestions on or off for this TUI (no argument toggles),
    /// and says what that means.
    pub(crate) fn toggle_suggest(&mut self, on: Option<bool>) {
        let on = on.unwrap_or(!self.view.suggest);
        self.view.suggest = on;
        if !on {
            self.suggestion = None;
            self.suggest_due = None;
        }
        self.suggest_failed = false;
        let calls = self.suggest_stats["calls"].as_u64().unwrap_or(0);
        self.view.status = Some(if on {
            format!(
                "suggestions on ({}): after a turn of the thread you see, its prompt and the end \
                 of its reply go to Anthropic with your Claude Code login · Tab or → takes one \
                 · {calls} asked so far",
                self.view.suggest_model
            )
        } else {
            "suggestions off".to_string()
        });
    }
}
