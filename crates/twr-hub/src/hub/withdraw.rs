//! Taking back a message (`message.withdraw {thread, ref}`).
//!
//! `say` and `thread.send` return the queued message's `ref`: its `pending_prompts` row id
//! (AUTOINCREMENT, never reused), which the turn it starts records as `ext.twr.ref`
//! ([`MESSAGE_REF_KEY`], stamped by `Core::dispatch_next`). A short command `say` applied at
//! once has no ref: nothing was queued.
//!
//! Under the core lock, so the queue, the turn and the first action are read as one:
//!
//! - **Still queued:** the row is removed in the same transaction as a `prompt.withdrawn
//!   {ref, prompt}` on its thread. What its arrival did stays done: confirm cards it
//!   superseded stay superseded, and the orchestrator turn it took authority from stays
//!   without it (`hub::orch`); both err on the safe side.
//! - **Its turn runs and has not acted:** `turn.withdrawn {ref, turn}` is recorded in that
//!   turn, then the runtime is told to interrupt it (the session stays). From the record on
//!   the turn has no authority: the relay guard refuses the orchestrator's relays and mode
//!   changes, and every other change it asks for ([`Core::orchestrator_acts`]); its end is
//!   recorded `interrupted` whatever the runtime reports (`hub::supervisor`), so it gets no
//!   report, review or retry; the orchestrator's handoff and conversation updates leave it
//!   out (`hub::handoff`, `hub::updates`), and the next turn the hub delivers a prompt for is
//!   told to disregard the message ([`Core::taken_back_fact`]): its runtime still has it.
//!   A turn the orchestrator's runtime starts on its own right after one (a continuation of
//!   the work) is taken back too unless a user message arrived since: no authority, and
//!   interrupted as below ([`Core::turn_taken_back`]).
//! - **Otherwise** it is refused with a reason: `acted` (the turn called a tool, raised a
//!   request, or, the orchestrator, asked the hub for a change: [`ThreadView`]'s
//!   `current_turn_acted`, [`Core::orch_turn_acted`]), `ended`, `uninterruptible` (its turn
//!   started on a runtime that cannot interrupt one: a pty, a remote box, a scripted fake),
//!   `withdrawn` (taken back already) or `unknown` (no such message of the thread).
//!
//! A runtime may ignore an interrupt that reaches it before it began the turn (the prompt was
//! delivered, the withdrawal came within milliseconds), and then run the whole turn. So while
//! a withdrawn turn is open, each kind of sign that its runtime runs it ([`RunningSign`]: its
//! own start, first text, thinking, tool call or request in it) interrupts it again, once per kind
//! ([`Core::withdrawn_turn_runs`]). A permission or question it raises waits for no one: the
//! hub declines it at once, recorded `attention.resolved{by: timeout, answer: deny,
//! ext.twr.taken_back: true}` (the hub's own closing of an item, as at a turn's end, with the
//! answer it forwarded: the runtime gets `deny`, which every adapter reads as declining a
//! permission or a question), so it is never on the user's stack.
//!
//! A withdrawal, the recording of an adapter's events and the orchestrator's calls take the
//! same lock, so a withdrawal and a turn's first action are ordered: whichever the hub sees
//! first wins. An action after the withdrawal is refused (the
//! orchestrator's calls) or comes too late to matter (a worker's tool call while the
//! interrupt is on its way: the turn ends anyway).
//!
//! [`ThreadView`]: twr_core::reduce::ThreadView

use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};

use super::supervisor::split_runtime_attn_id;
use super::{append_err, internal, rpc, twr_ext_map, Batch, Caller, Core, Hub, Inner, LOCAL_BOX};
use crate::store;

/// The orchestrator a call it makes is bound to, as the hub saw it when the request came in
/// ([`Hub::orchestrator_call`], [`Core::orchestrator_acts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchCall {
    /// The orchestrator thread.
    pub(super) thread: String,
    /// Its live runtime's generation (`None`: none ran).
    pub(super) generation: Option<u64>,
    /// Its open turn.
    pub(super) turn: Option<u64>,
    /// The turns it had started: between turns, the last one is this one.
    pub(super) turns: u64,
}

impl OrchCall {
    /// The turn the call came in, if one was open.
    pub fn turn(&self) -> Option<u64> {
        self.turn
    }
}

/// Why `message.withdraw` refused, as its result's `reason` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The message's turn already acted on it.
    Acted,
    /// The message's turn already ended.
    Ended,
    /// Its turn started on a runtime that cannot interrupt a turn.
    Uninterruptible,
    /// Taken back already.
    Withdrawn,
    /// The thread has no such message.
    Unknown,
}

impl Refusal {
    /// The `reason` string.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Refusal::Acted => "acted",
            Refusal::Ended => "ended",
            Refusal::Uninterruptible => "uninterruptible",
            Refusal::Withdrawn => "withdrawn",
            Refusal::Unknown => "unknown",
        }
    }
}

/// A sign, in an adapter's event, that its runtime runs the open turn ([`Core::withdrawn_turn_runs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RunningSign {
    /// A turn the runtime started on its own (`turn.started{from: runtime}`).
    Started,
    Text,
    Thinking,
    ToolCall,
    /// A permission or question, by its (namespaced) attention id.
    Request(String),
}

impl RunningSign {
    /// The sign `body` is, if any.
    pub(super) fn of(body: &EventBody) -> Option<RunningSign> {
        match body {
            EventBody::TurnStarted(p) if p.from == TurnFrom::Runtime => Some(RunningSign::Started),
            EventBody::AgentText(_) => Some(RunningSign::Text),
            EventBody::AgentThinking(_) => Some(RunningSign::Thinking),
            EventBody::AgentToolCall(_) => Some(RunningSign::ToolCall),
            EventBody::AttentionRaised(p) if p.reason.is_blocking() => {
                Some(RunningSign::Request(p.attn_id.clone()))
            }
            _ => None,
        }
    }

    /// Its bit among the kinds a turn already showed.
    fn bit(&self) -> u8 {
        match self {
            RunningSign::Text => 1,
            RunningSign::Thinking => 2,
            RunningSign::ToolCall => 4,
            RunningSign::Request(_) => 8,
            RunningSign::Started => 16,
        }
    }
}

impl Hub {
    /// `message.withdraw {thread, ref}` (users only): takes back message `msg_ref` of thread
    /// `query` (see the module docs). Returns `{withdrawn: true, state: "queued" | "running",
    /// thread, thread_name, ref, prompt, turn?}`, or `{withdrawn: false, reason, message,
    /// thread, thread_name, ref, turn?}` when it is too late (reasons: [`Refusal`]).
    pub fn withdraw_message(
        &self,
        query: &str,
        msg_ref: i64,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user takes back a message",
            ));
        }
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        core.withdraw_locked(&self.inner, &id, msg_ref, caller)
    }

    /// The orchestrator asks for a change through the hub (`method`), received when it was
    /// `received` ([`Hub::orchestrator_call`] when the request came in): refused when it was
    /// replaced or restarted since, or the user took back the message of the turn it came in
    /// (or of the last one, between turns); else that turn has acted
    /// ([`Core::orchestrator_acts`]). The server calls it before every orchestrator call that
    /// changes something.
    pub fn orchestrator_acts(
        &self,
        method: &str,
        received: Option<&OrchCall>,
    ) -> Result<(), RpcError> {
        self.inner.core().orchestrator_acts(method, received)
    }

    /// The orchestrator as a call it makes now is bound to; `None` without one.
    pub fn orchestrator_call(&self) -> Option<OrchCall> {
        self.inner.core().orchestrator_call()
    }
}

impl Core {
    /// Thread `thread`'s runtime just showed, in turn `turn`, that it runs it (`sign`, an
    /// event recorded in it). When that turn is open and its message was taken back: a request
    /// is declined at once (see the module docs), and the first sign of each kind interrupts
    /// the turn again. Failures are logged, never returned: the event is recorded already.
    pub(super) fn withdrawn_turn_runs(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        sign: RunningSign,
    ) {
        if !self.turn_taken_back(thread, turn) {
            return;
        }
        if let RunningSign::Request(attn_id) = &sign {
            self.decline_taken_back(hub, thread, turn, attn_id);
        }
        let rt = self.rt(thread);
        let shown = match rt.reinterrupted {
            Some((t, shown)) if t == turn => shown,
            _ => 0,
        };
        if shown & sign.bit() != 0 {
            return;
        }
        rt.reinterrupted = Some((turn, shown | sign.bit()));
        if let Some(live) = rt.live.as_ref() {
            tracing::info!(thread, turn, ?sign, "interrupting a taken-back turn again");
            if live.handle.interrupt().is_err() {
                tracing::info!(thread, turn, "the runtime was gone before the interrupt");
            }
        }
    }

    /// Declines request `attn_id` that a turn whose message was taken back raised: recorded,
    /// then `deny` goes to the runtime.
    fn decline_taken_back(&mut self, hub: &Inner, thread: &str, turn: u64, attn_id: &str) {
        let resolved = EventBody::AttentionResolved(AttentionResolved {
            attn_id: attn_id.to_string(),
            by: ResolvedBy::Timeout,
            answer: Some(Answer::deny()),
            ext: twr_ext_map(json!({ "taken_back": true })),
            ..Default::default()
        });
        if let Err(e) = self.append(hub, thread, Some(turn), Source::Hub, resolved) {
            tracing::warn!(thread, attn_id, error = %format!("{e:#}"), "could not decline a taken-back turn's request; its turn's end closes it");
            return;
        }
        let runtime_id = split_runtime_attn_id(thread, attn_id).map(|(_, id)| id.to_string());
        if let (Some(id), Some(live)) = (
            runtime_id,
            self.rt.get(thread).and_then(|r| r.live.as_ref()),
        ) {
            if live.handle.respond(id, Answer::deny()).is_err() {
                tracing::info!(thread, attn_id, "the runtime was gone before the answer");
            }
        }
    }

    fn withdraw_locked(
        &mut self,
        hub: &Inner,
        id: &str,
        msg_ref: i64,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let name = self.thread_name(id);
        let base = json!({"thread": id, "thread_name": name, "ref": msg_ref});
        let with = |mut v: Value, more: Value| {
            if let (Some(v), Some(more)) = (v.as_object_mut(), more.as_object()) {
                v.extend(more.clone());
            }
            v
        };
        // Still queued: it goes with its record, in one transaction.
        let queued = self
            .store
            .pending_prompt(msg_ref)
            .map_err(internal)?
            .filter(|p| p.thread == id);
        if let Some(p) = queued {
            let mut batch = Batch {
                dequeue: Some(p.id),
                ..Batch::default()
            };
            batch.push(
                id,
                None,
                caller.source(),
                EventBody::PromptWithdrawn(PromptWithdrawn {
                    msg_ref,
                    prompt: p.prompt.clone(),
                    ..Default::default()
                }),
            );
            self.append_batch(hub, batch).map_err(append_err)?;
            return Ok(with(
                base,
                json!({"withdrawn": true, "state": "queued", "prompt": p.prompt}),
            ));
        }
        let view = self.view(id)?.clone();
        let refuse = |why: Refusal, turn: Option<u64>| {
            let message = match why {
                Refusal::Acted if view.is_orchestrator() => {
                    "too late: the orchestrator already acted on it".to_string()
                }
                Refusal::Acted => {
                    format!("too late: {name} already started on it; `twr stop {name}` stops it")
                }
                Refusal::Ended => "too late: its turn already ended".to_string(),
                Refusal::Uninterruptible => format!(
                    "{name}'s runtime cannot interrupt a turn, and this message's turn started; \
                     `twr stop {name}` stops it"
                ),
                Refusal::Withdrawn => "it was taken back already".to_string(),
                Refusal::Unknown => format!("{name} has no message {msg_ref} to take back"),
            };
            Ok(with(
                base.clone(),
                json!({"withdrawn": false, "reason": why.as_str(), "message": message,
                       "turn": turn}),
            ))
        };
        let running = view
            .current_turn
            .filter(|_| view.current_turn_ref == Some(msg_ref));
        let Some(turn) = running else {
            // Not queued, not running: it started and ended, or it never was the thread's.
            let started =
                store::turn_of_message(self.store.conn(), id, msg_ref).map_err(internal)?;
            let withdrawn =
                store::message_withdrawn(self.store.conn(), id, msg_ref).map_err(internal)?;
            return match (withdrawn, started) {
                (true, t) => refuse(Refusal::Withdrawn, t),
                (false, Some(t)) => refuse(Refusal::Ended, Some(t)),
                (false, None) => refuse(Refusal::Unknown, None),
            };
        };
        if view.current_turn_withdrawn {
            return refuse(Refusal::Withdrawn, Some(turn));
        }
        if view.current_turn_acted || self.orch_turn_acted(id, turn) {
            return refuse(Refusal::Acted, Some(turn));
        }
        // The live process's runtime decides, not the thread's: a thread moved to another
        // runtime finishes its turn on the previous one (`hub::restart`). Unknown: refused.
        let rt = self.rt.get(id);
        let live_runtime = rt
            .filter(|r| r.live.is_some())
            .and_then(|r| r.runtime_settings.as_ref())
            .map(|s| s.runtime.as_str());
        let interruptible = view.box_name == LOCAL_BOX
            && live_runtime.is_some_and(|runtime| {
                hub.host(&view.box_name)
                    .is_some_and(|h| h.interrupts_turns(runtime))
            });
        if !interruptible {
            return refuse(Refusal::Uninterruptible, Some(turn));
        }
        // Recorded first: from here on the turn has no authority, whatever the runtime does
        // before the interrupt reaches it.
        let mut batch = Batch::default();
        batch.push(
            id,
            Some(turn),
            caller.source(),
            EventBody::TurnWithdrawn(TurnWithdrawn {
                msg_ref,
                turn,
                ..Default::default()
            }),
        );
        self.append_batch(hub, batch).map_err(append_err)?;
        if let Some(live) = self.rt.get(id).and_then(|r| r.live.as_ref()) {
            if live.handle.interrupt().is_err() {
                // The runtime is exiting: its closing sequence ends the turn interrupted.
                tracing::info!(thread = %id, turn, "the runtime was gone before the interrupt");
            }
        }
        let prompt = store::last_of_kind(self.store.conn(), id, Some(turn), "turn.started")
            .ok()
            .flatten()
            .map(|e| self.full(e))
            .and_then(|e| match e.body {
                EventBody::TurnStarted(p) => Some(p.prompt),
                _ => None,
            });
        Ok(with(
            base,
            json!({"withdrawn": true, "state": "running", "turn": turn, "prompt": prompt}),
        ))
    }
}
