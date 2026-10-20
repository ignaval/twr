//! The queue of a thread's messages as the user sees and changes it.
//!
//! A message sent to a busy thread waits in `pending_prompts` (`hub::store`), one row per
//! message, in the order they run; its row id is the message's `ref`. The hub keeps that queue,
//! and clients read it here:
//!
//! - `thread.queue {thread}` lists what waits, in order, each with its ref, sender and the
//!   start of its text ([`QUEUE_LIST_MAX`] of them, [`QUEUE_PREVIEW_MAX`] bytes each).
//! - Taking one back is `message.withdraw` (`hub::withdraw`); editing one is taking it back
//!   and sending the edited text as a new message (`say`, `thread.send`): it arrives anew,
//!   with what the user has seen when sending it, at the end of the queue, and ends what an
//!   earlier message's turn could still do as any new message does (`hub::orch`).
//! - `message.send_now {thread, ref}` runs the next queued message now: the turn that runs is
//!   interrupted (`turn.interrupted {ref, turn}` recorded in it first) and the message is the
//!   next prompt. The interrupted turn keeps what it did, and its authority, as any turn the
//!   user interrupts; it ends `interrupted` whatever its runtime reports, so it gets no
//!   report, review or retry (`hub::supervisor`), and while it is open each kind of sign that
//!   its runtime still runs it interrupts it again (`hub::withdraw`). A failure that held the
//!   queue is released, as the user's own send releases it. Only the next message: the queue
//!   keeps its order, so a later one would wait behind the others anyway.
//!
//! Users only: the orchestrator and agents get `unauthorized` (the server's method lists, and
//! the checks here).

use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::ThreadView;

use super::{append_err, internal, rpc, Batch, Caller, Core, Hub, Inner, LOCAL_BOX};

/// Most queued messages `thread.queue` lists (`total` counts them all).
pub const QUEUE_LIST_MAX: usize = 20;

/// Most bytes of a queued message's text `thread.queue` shows (`truncated` says when cut):
/// enough for a row of the TUI, not the whole message, which `message.withdraw` returns.
pub const QUEUE_PREVIEW_MAX: usize = 2048;

/// The start of `s`, at most `max` bytes, cut on a character boundary.
fn preview(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

impl Hub {
    /// `thread.queue {thread}` (users only): thread `query`'s queued messages, in the order
    /// they run. `{thread, thread_name, total, held, newest_mine, items: [{ref, prompt,
    /// truncated, from, mine, queued_at}]}`: `mine` is a message the user sent (from a client
    /// of theirs), `newest_mine` the ref of the newest of those in the whole queue (the list
    /// holds the first [`QUEUE_LIST_MAX`]), `held` says a failure holds the queue until the
    /// user's next send.
    pub fn thread_queue(&self, query: &str, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user reads a thread's queue",
            ));
        }
        let core = self.inner.core();
        let id = core.resolve(query)?;
        let total = core.store.pending_count(&id).map_err(internal)?;
        let rows = core
            .store
            .pending_list(&id, QUEUE_LIST_MAX)
            .map_err(internal)?;
        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                let (text, truncated) = preview(&r.prompt, QUEUE_PREVIEW_MAX);
                let mine = r.from == TurnFrom::User
                    && matches!(r.source, twr_core::event::Source::User(_));
                json!({
                    "ref": r.id,
                    "prompt": text,
                    "truncated": truncated,
                    "from": r.from,
                    "mine": mine,
                    "queued_at": r.queued_at,
                })
            })
            .collect();
        let newest_mine = core.store.newest_user_pending(&id).map_err(internal)?;
        Ok(json!({
            "thread": id,
            "thread_name": core.thread_name(&id),
            "total": total,
            "held": core.rt.get(&id).is_some_and(|r| r.queue_held),
            "items": items,
            "newest_mine": newest_mine,
        }))
    }

    /// `message.send_now {thread, ref}` (users only): runs queued message `msg_ref` of thread
    /// `query` next, now (see the module docs). `{sent_now: true, state: "interrupting" |
    /// "started" | "waiting", thread, thread_name, ref, turn?, why?}`, or `{sent_now: false,
    /// reason, message, ...}` with `reason` `unknown` (not queued: it started, was taken back
    /// or never was), `not_next` (other messages are ahead of it) or `uninterruptible` (the
    /// turn that runs can't be interrupted: it runs once that turn ends).
    pub fn send_now(&self, query: &str, msg_ref: i64, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user sends a queued message now",
            ));
        }
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        core.send_now_locked(&self.inner, &id, msg_ref, caller)
    }
}

impl Core {
    /// Whether the open turn of thread `id` (`view`) can be interrupted: its live runtime is
    /// on the hub's own box and interrupts turns (a pty, a remote box, a scripted fake don't).
    /// The live process's runtime decides, not the thread's: a thread moved to another runtime
    /// finishes its turn on the previous one (`hub::restart`). Unknown: no.
    pub(super) fn turn_interruptible(&self, hub: &Inner, id: &str, view: &ThreadView) -> bool {
        let live_runtime = self
            .rt
            .get(id)
            .filter(|r| r.live.is_some())
            .and_then(|r| r.runtime_settings.as_ref())
            .map(|s| s.runtime.as_str());
        view.box_name == LOCAL_BOX
            && live_runtime.is_some_and(|runtime| {
                hub.host(&view.box_name)
                    .is_some_and(|h| h.interrupts_turns(runtime))
            })
    }

    /// Whether turn `turn` of `thread` is open and the user interrupted it to send the next
    /// queued message now (`turn.interrupted`).
    pub(super) fn turn_interrupted(&self, thread: &str, turn: u64) -> bool {
        self.registry
            .get(thread)
            .is_some_and(|v| v.current_turn == Some(turn) && v.current_turn_interrupted)
    }

    /// The user asked for queued message `msg_ref` of thread `id` to run now: a failure that
    /// held the queue lets it go, as their own send does, and durably (the row is marked the
    /// user's ask, `user_sent`), so a hub started after a crash doesn't hold it again.
    fn release_for(&mut self, id: &str, msg_ref: i64) -> Result<(), RpcError> {
        if self.rt.get(id).is_some_and(|r| r.queue_held) {
            let head = self.store.control_head().map_err(internal)?;
            self.store.mark_user_sent(msg_ref, head).map_err(internal)?;
        }
        self.release_queue(id);
        Ok(())
    }

    fn send_now_locked(
        &mut self,
        hub: &Arc<Inner>,
        id: &str,
        msg_ref: i64,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let name = self.thread_name(id);
        let base = json!({"thread": id, "thread_name": name, "ref": msg_ref});
        let with = |more: Value| {
            let mut v = base.clone();
            if let (Some(v), Some(more)) = (v.as_object_mut(), more.as_object()) {
                v.extend(more.clone());
            }
            v
        };
        let queued = self
            .store
            .pending_prompt(msg_ref)
            .map_err(internal)?
            .filter(|p| p.thread == id);
        if queued.is_none() {
            return Ok(with(json!({
                "sent_now": false,
                "reason": "unknown",
                "message": format!(
                    "{name} has no message {msg_ref} queued: it started, or was taken back"
                ),
            })));
        }
        let ahead = self
            .store
            .pending_list(id, usize::MAX)
            .map_err(internal)?
            .iter()
            .take_while(|p| p.id != msg_ref)
            .count();
        if ahead > 0 {
            return Ok(with(json!({
                "sent_now": false,
                "reason": "not_next",
                "message": format!(
                    "{ahead} queued message{} ahead of it: only the next one can be sent now",
                    if ahead == 1 { " is" } else { "s are" }
                ),
            })));
        }
        let view = self.view(id)?.clone();
        if let Some(turn) = view.current_turn {
            if view.current_turn_interrupted || view.current_turn_withdrawn {
                self.release_for(id, msg_ref)?;
                return Ok(with(json!({
                    "sent_now": true, "state": "interrupting", "turn": turn,
                })));
            }
            if !self.turn_interruptible(hub, id, &view) {
                return Ok(with(json!({
                    "sent_now": false,
                    "reason": "uninterruptible",
                    "turn": turn,
                    "message": format!(
                        "{name}'s runtime cannot interrupt a turn: the message runs when this \
                         one ends (`twr stop {name}` stops it)"
                    ),
                })));
            }
            // Recorded first: its end is `interrupted` whatever the runtime does before the
            // interrupt reaches it.
            let mut batch = Batch::default();
            batch.push(
                id,
                Some(turn),
                caller.source(),
                EventBody::TurnInterrupted(TurnInterrupted {
                    msg_ref,
                    turn,
                    ..Default::default()
                }),
            );
            self.append_batch(hub, batch).map_err(append_err)?;
            self.release_for(id, msg_ref)?;
            if let Some(live) = self.rt.get(id).and_then(|r| r.live.as_ref()) {
                if live.handle.interrupt().is_err() {
                    // The runtime is exiting: its closing sequence ends the turn.
                    tracing::info!(thread = %id, turn, "the runtime was gone before the interrupt");
                }
            }
            return Ok(with(json!({
                "sent_now": true, "state": "interrupting", "turn": turn,
            })));
        }
        // No turn runs: the queue waits on something else (a failure that held it, an attach,
        // a park, an offline box, a runtime being stopped). The user's ask releases a held
        // queue; the rest stays in the way.
        self.release_for(id, msg_ref)?;
        let started = self
            .dispatch_next(hub, id)
            .map_err(|e| rpc(codes::INTERNAL, format!("{e:#}")))?;
        Ok(match started {
            Some((turn, Some(row))) if row == msg_ref => with(json!({
                "sent_now": true, "state": "started", "turn": turn,
            })),
            _ => with(json!({
                "sent_now": true,
                "state": "waiting",
                "message": format!(
                    "{name} can't start a turn now (parked, attached, its box offline, or its \
                     runtime stopping or retrying): the message is next"
                ),
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_cut_on_character_boundaries() {
        assert_eq!(preview("short", 10), ("short", false));
        assert_eq!(preview("abcdef", 3), ("abc", true));
        // "é" is two bytes: a cut inside it backs off to before it.
        assert_eq!(preview("aé", 2), ("a", true));
    }
}
