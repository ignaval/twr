//! The queue of the main pane's thread, as the hub keeps it (`thread.queue`): the messages
//! sent to it while it was busy, in the order they run. The TUI reads it when something about
//! it changes (a message sent or taken back, a turn starting or ending on that thread, another
//! thread on the main pane) and, while the thread works or messages wait, every
//! [`QUEUE_POLL`], so what other clients and the orchestrator queue shows up too.
//!
//! It lists them above the input box ([`crate::ui`]). A message of the user's is edited by
//! taking it back into the input box (`message.withdraw`, the full text coming back from the
//! hub, never the list's preview) and sending it again: it is then a new message, at the end
//! of the queue. `↑` on an empty line does that to the newest; a queued row, selected with a
//! click or with the conversation's Shift-↓, opens an action popup that edits it, deletes it
//! or, for the next one, sends it now (`message.send_now`).

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::app::{Action, ActionTarget, AppState, Retract, Screen, Withdrawal};

/// How often the queue is read again while its thread works or messages wait.
pub const QUEUE_POLL: Duration = Duration::from_secs(2);

/// How long after a failed read (an older hub without `thread.queue`, a dropped connection)
/// the next one waits.
pub const QUEUE_RETRY: Duration = Duration::from_secs(30);

/// A read on its way longer than this is taken as lost: the next one may go.
const QUEUE_ASK_TIMEOUT: Duration = Duration::from_secs(10);

/// Most queued messages listed above the input box; a row says how many more wait.
pub const QUEUE_ROWS: usize = 3;

/// A queued message as `thread.queue` listed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    /// Its ref: what taking it back or sending it now names.
    pub msg_ref: i64,
    /// The start of its text ([`Queued::truncated`] when cut).
    pub prompt: String,
    /// The text was cut to a preview.
    pub truncated: bool,
    /// Who sent it: `user`, `orchestrator` or `runtime` (a job's wake).
    pub from: String,
    /// The user sent it, from a client of theirs: it can be edited.
    pub mine: bool,
}

/// The main pane's thread's queue ([`AppState::queue`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueView {
    /// The thread.
    pub thread: String,
    /// What waits, in the order it runs (the first [`QUEUE_ROWS`] or so the hub listed).
    pub items: Vec<Queued>,
    /// How many wait in all.
    pub total: u64,
    /// A failure holds the queue until the user's next send.
    pub held: bool,
    /// A read is on its way, since then.
    pub asked: Option<Instant>,
    /// The last read answered then.
    pub fetched: Option<Instant>,
    /// Something changed since the last read: read again at once.
    pub dirty: bool,
    /// The last read failed: the next waits [`QUEUE_RETRY`].
    pub failed: bool,
}

impl QueueView {
    fn new(thread: &str) -> QueueView {
        QueueView {
            thread: thread.to_string(),
            dirty: true,
            ..QueueView::default()
        }
    }
}

/// The items of a `thread.queue` result.
fn items(result: &Value) -> Vec<Queued> {
    result["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    Some(Queued {
                        msg_ref: i["ref"].as_i64()?,
                        prompt: i["prompt"].as_str().unwrap_or_default().to_string(),
                        truncated: i["truncated"].as_bool().unwrap_or(false),
                        from: i["from"].as_str().unwrap_or("user").to_string(),
                        mine: i["mine"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl AppState {
    /// The queue the main pane's thread has, while there is something in it to list: in the
    /// stack view (the compact layout's conversation included) only.
    pub fn shown_queue(&self) -> Option<&QueueView> {
        let main = self.main_thread()?;
        self.queue
            .as_ref()
            .filter(|q| q.thread == main && !q.items.is_empty())
            .filter(|_| self.view.screen == Screen::Stack)
    }

    /// The queued message selected ([`crate::app::ViewState::queued`]), while it is listed.
    pub fn selected_queued(&self) -> Option<&Queued> {
        let r = self.view.queued?;
        self.shown_queue()?.items.iter().find(|i| i.msg_ref == r)
    }

    /// Something about thread `thread`'s queue changed (a message sent or taken back, a turn
    /// started or ended): when the main pane shows it, read the queue again at once.
    pub fn queue_changed(&mut self, thread: &str) {
        if let Some(q) = self.queue.as_mut().filter(|q| q.thread == thread) {
            q.dirty = true;
        }
    }

    /// When the queue is to be read next (`None`: not until something changes): at once
    /// when the main pane shows another thread or something changed, [`QUEUE_POLL`] after
    /// the last read while the thread works or messages wait, [`QUEUE_RETRY`] after a failed
    /// one; not while a read is on its way.
    pub fn queue_deadline(&self, now: Instant) -> Option<Instant> {
        let main = self.main_thread()?;
        let Some(q) = self.queue.as_ref().filter(|q| q.thread == main) else {
            return Some(now);
        };
        if let Some(at) = q.asked {
            return Some(at + QUEUE_ASK_TIMEOUT);
        }
        let after = |d: Duration| Some(q.fetched.map_or(now, |f| f + d));
        if q.failed {
            return after(QUEUE_RETRY);
        }
        if q.dirty {
            return Some(now);
        }
        let busy = self
            .registry
            .get(&main)
            .is_some_and(|v| v.current_turn.is_some());
        if busy || !q.items.is_empty() || q.total > 0 {
            return after(QUEUE_POLL);
        }
        None
    }

    /// The thread whose queue to read now, if a read is due ([`AppState::queue_deadline`]);
    /// the read is then on its way. A new thread on the main pane starts with an empty list
    /// and no queued row selected.
    pub fn queue_due(&mut self, now: Instant) -> Option<String> {
        let main = self.main_thread()?;
        if self.queue.as_ref().is_none_or(|q| q.thread != main) {
            self.queue = Some(QueueView::new(&main));
            self.view.queued = None;
        }
        if self.queue_deadline(now).is_none_or(|d| d > now) {
            return None;
        }
        let q = self.queue.as_mut()?;
        q.asked = Some(now);
        q.dirty = false;
        Some(main)
    }

    /// What `thread.queue` returned (`Value::Null`: the read failed): kept when it is about
    /// the main pane's thread; a selected row no longer queued is released.
    pub fn queue_result(&mut self, result: &Value, now: Instant) {
        let Some(q) = self.queue.as_mut() else {
            return;
        };
        if result.is_null() || result["failed"] == true {
            q.asked = None;
            q.fetched = Some(now);
            q.failed = true;
            return;
        }
        if result["thread"].as_str() != Some(q.thread.as_str()) {
            return;
        }
        q.asked = None;
        q.fetched = Some(now);
        q.failed = false;
        q.items = items(result);
        q.total = result["total"].as_u64().unwrap_or(q.items.len() as u64);
        q.held = result["held"].as_bool().unwrap_or(false);
        if self.view.queued.is_some() && self.selected_queued().is_none() {
            self.view.queued = None;
        }
    }

    /// `↑` on an empty line with messages of the user's queued on the main pane's thread:
    /// takes the newest back into the input box, to edit and send again (`None`: none).
    pub(crate) fn edit_newest_queued(&mut self) -> Option<Vec<Action>> {
        if !self.view.input.is_empty() {
            return None;
        }
        let q = self.shown_queue()?;
        let newest = q.items.iter().rev().find(|i| i.mine)?.msg_ref;
        Some(self.retract_queued(newest, Retract::Edit))
    }

    /// Takes queued message `msg_ref` of the main pane's thread back (`message.withdraw`),
    /// to edit it ([`Retract::Edit`]: its whole text goes back in the input box) or to delete
    /// it ([`Retract::Delete`]: its text goes to the history).
    pub(crate) fn retract_queued(&mut self, msg_ref: i64, purpose: Retract) -> Vec<Action> {
        let Some(q) = self.shown_queue() else {
            return Vec::new();
        };
        let Some(item) = q.items.iter().find(|i| i.msg_ref == msg_ref) else {
            return Vec::new();
        };
        let thread = q.thread.clone();
        if self.withdrawals.iter().any(|w| w.msg_ref == msg_ref) {
            return Vec::new();
        }
        self.withdrawals.push(Withdrawal {
            thread: thread.clone(),
            msg_ref,
            text: item.prompt.clone(),
            purpose,
        });
        if self.view.queued == Some(msg_ref) {
            self.view.queued = None;
        }
        self.view.status = Some(match purpose {
            Retract::Delete => "deleting the queued message…".into(),
            _ => "taking the queued message back to edit it…".into(),
        });
        vec![Action::Withdraw { thread, msg_ref }]
    }

    /// Sends queued message `msg_ref` of the main pane's thread now (`message.send_now`).
    pub(crate) fn send_queued_now(&mut self, msg_ref: i64) -> Vec<Action> {
        let Some(q) = self.shown_queue() else {
            return Vec::new();
        };
        let thread = q.thread.clone();
        self.view.queued = None;
        vec![Action::SendNow { thread, msg_ref }]
    }

    /// What `message.send_now` returned: the status bar says what happens, and the queue is
    /// read again.
    pub(crate) fn send_now_result(&mut self, result: &Value) {
        if let Some(t) = result["thread"].as_str() {
            let t = t.to_string();
            self.queue_changed(&t);
        }
        let name = result["thread_name"].as_str().unwrap_or("the thread");
        let msg = match (result["sent_now"].as_bool(), result["state"].as_str()) {
            (Some(true), Some("interrupting")) => {
                format!("interrupting {name}'s turn: your queued message runs next")
            }
            (Some(true), Some("started")) => format!("sent to {name} now"),
            (Some(true), _) => result["message"]
                .as_str()
                .unwrap_or("it is next in the queue")
                .to_string(),
            _ => result["message"]
                .as_str()
                .unwrap_or("it can't be sent now")
                .to_string(),
        };
        self.set_timed_status(msg, Instant::now());
    }

    /// Selects queued message `msg_ref` (a click, the conversation's Shift-↓): the
    /// conversation's other selections go.
    pub(crate) fn select_queued(&mut self, msg_ref: i64) {
        self.view.queued = Some(msg_ref);
        self.view.block = None;
        self.view.request = None;
        self.view.main_focus = true;
    }

    /// The action popup's target for the selected queued message.
    pub(crate) fn queued_target(&self) -> Option<ActionTarget> {
        self.selected_queued()
            .map(|i| ActionTarget::Queued(i.msg_ref))
    }
}
