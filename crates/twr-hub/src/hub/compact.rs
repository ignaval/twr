//! Compaction of a thread's context (`thread.compact`: the TUI's `/compact-context`, `twr
//! compact`). Who asks for one:
//!
//! - the user: the method is in neither the orchestrator's nor an agent's set (`server`), the
//!   orchestrator's identity is refused here as well, and a prompt of anyone else whose first
//!   word is `/compact` (which Claude Code and Grok Build would run as the command) is refused
//!   where prompts are queued ([`refuse_compact_command`]);
//! - the user, through the hub's offer before a message goes to a worker whose prompt cache
//!   expired (`Compact first`, `hub::idle_compact`): only the user answers that card, and the
//!   compaction is the user's (their source), recorded with `ext.twr.compact_by: "offer"`;
//! - the hub itself, only when the user configured `[threads] idle_compaction =
//!   "before_expiry"`: an idle worker's context just before its prompt cache expires
//!   (`hub::idle_compact`), with `source: hub` and `ext.twr.compact_by: "idle"`.
//!
//! Never the orchestrator or an agent. The user's own `thread.compact` records no
//! `compact_by` ([`CompactBy::User`]), as before it existed.
//!
//! A compaction is a turn of its own: `turn.started {prompt: "/compact", from: user}` with
//! `ext.twr.compact` ([`COMPACT_TURN_KEY`]), sent to the runtime as [`AdapterCmd::Compact`]
//! with nothing prepended (no standing context, handoff or hub facts), and ended by the
//! runtime as any turn, on the thread's account: a runtime it starts for it is started as for
//! any turn (`Core::spawn_config`: the account's directory in the runtime's variable, the
//! start refused on a removed account or one whose directory changed). Like a prompt it starts
//! only between turns (a Codex compaction sent
//! during a turn would interrupt it), after the messages queued before it was asked for: the
//! ask waits in memory ([`CompactAsk`]), so a hub restart before it starts forgets it, and is
//! checked again when it is due: one asked of a session that is no longer the thread's (a
//! move to another runtime, a new session) is dropped. It is consumed once its `turn.started`
//! is recorded. While the hub's offer to compact before a held message is open
//! (`hub::idle_compact`), the user's `thread.compact` answers it `Compact first` (recorded as
//! their answer): the compaction then runs before the messages the card held.
//!
//! A compaction is maintenance, not work (`ThreadView::current_turn_compact`), whoever asked
//! for it: no grade, report or review ([`Core::react_to_compaction_end`]; one that failed, by
//! its turn's end or its own `context.compacted` `error`, raises an `error` item and is not
//! retried), the thread's last turn and report stay its work's (so watches, `send
//! --after`, wakes and retries count it as nothing, and work that ends while it runs wakes the
//! worker after it, `hub::wake`, or fires the watch on it going idle, `hub::watches`), it tells
//! the orchestrator no fact of a message taken back, is no message of the user's, and leaves a
//! taken-back turn's taint to what follows it (`hub::orch`), no reply to notify (`twr_core::notify`) and
//! no exchange to suggest a next message after (`hub::suggest`).
//!
//! Refused (`bad_request`, naming why): a runtime that cannot compact from twr
//! ([`ThreadHost::compacts`]: Claude Code, Codex and Grok Build can; a generic ACP agent, a pty
//! program and the fake runtime cannot), a thread on a remote box (the runner protocol has no
//! such command yet), a thread with no session yet or waiting for its new runtime's first one,
//! and a parked or attached one.
//!
//! `thread.compact {thread, offers: "on" | "off"}` (the user only) compacts nothing: it turns
//! the hub's offers for the thread on or off (`hub::idle_compact`), recorded as a
//! `conversation.update` on it with `ext.twr.idle_compaction`.
//!
//! [`AdapterCmd::Compact`]: twr_core::adapter::AdapterCmd::Compact
//! [`ThreadHost::compacts`]: crate::host::ThreadHost::compacts

use std::sync::Arc;

use serde_json::{json, Map, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};

use super::{internal, rpc, twr_ext_map, Batch, Caller, Core, Hub, Inner};

/// Whether `prompt` is a runtime's compaction command: its first word is `/compact` (any case;
/// Claude Code also takes `/compact <instructions>`). Delivered as a prompt, Claude Code and
/// Grok Build run it as the command.
pub(super) fn is_compact_command(prompt: &str) -> bool {
    prompt
        .split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("/compact"))
}

/// Refuses prompt `prompt` from `from` when it is a compaction command ([`is_compact_command`]).
/// Only the user compacts a thread's context: a prompt of the orchestrator's (a send, a
/// thread's first prompt, a send held for other threads) or of the hub's (a job's wake) is no
/// way around that (`unauthorized`). The user's own goes the compaction's way instead
/// (`bad_request` pointing to it): as a prompt it would compact the context in a turn that
/// counts as work (graded, reported, finishing what waits on the thread), with no record of
/// the compaction's start for a runtime that reports none, and `thread.compact` takes no
/// instructions. Checked where every prompt is queued.
pub(super) fn refuse_compact_command(prompt: &str, from: TurnFrom) -> Result<(), RpcError> {
    if !is_compact_command(prompt) {
        return Ok(());
    }
    if from == TurnFrom::User {
        return Err(rpc(
            codes::BAD_REQUEST,
            "a message that starts with /compact would compact the thread's context as a turn \
             of work: compact it with `twr compact <thread>` (/compact-context in the TUI) \
             instead, which runs it as maintenance",
        ));
    }
    Err(rpc(
        codes::UNAUTHORIZED,
        "a prompt that starts with /compact compacts the thread's context, which only the user \
         does (twr compact, /compact-context in the TUI); tell the user",
    ))
}

/// `ext.twr` key of a compaction's `turn.started`: why it runs when the user did not ask for
/// it with `thread.compact` ([`CompactBy`]).
pub(super) const COMPACT_BY_KEY: &str = "compact_by";

/// What asked for a compaction (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompactBy {
    /// The user's `thread.compact`.
    User,
    /// The user's `Compact first` on the hub's offer (`hub::idle_compact`).
    Offer,
    /// The hub, before the prompt cache expired, under `[threads] idle_compaction =
    /// "before_expiry"` (`hub::idle_compact`).
    Idle,
}

impl CompactBy {
    /// Its `ext.twr.compact_by`: none for the user's own ask.
    fn recorded(self) -> Option<&'static str> {
        match self {
            CompactBy::User => None,
            CompactBy::Offer => Some("offer"),
            CompactBy::Idle => Some("idle"),
        }
    }
}

/// A compaction asked for that has not started yet ([`super::ThreadRt::compact`]).
#[derive(Debug, Clone)]
pub(super) struct CompactAsk {
    /// The last queue row when it was asked for: the messages queued before it go first.
    pub(super) after: i64,
    /// Who asked (the turn's source).
    pub(super) source: Source,
    /// The thread's runtime and session when it was asked for: a compaction is of that
    /// session's context, and is dropped when it is no longer the thread's (a move to another
    /// runtime, whose first turn starts a new session, a new session of the same one).
    pub(super) runtime: String,
    pub(super) session: Option<String>,
    /// What asked for it.
    pub(super) by: CompactBy,
}

impl Hub {
    /// `thread.compact {thread}`: compacts thread `query`'s context now, or once its running
    /// turn and the messages queued before it are done (see the module docs). Returns
    /// `{thread, thread_name, state: started | queued, turn?}`.
    pub fn compact_thread(&self, query: &str, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user compacts a thread's context; tell the user",
            ));
        }
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        core.compact_locked(&self.inner, &id, caller)
    }

    /// `thread.compact {thread, offers}`: turns the hub's offers to compact thread `query`'s
    /// context once its prompt cache expired on (`"on"`) or off (`"off"`), compacting nothing
    /// (`hub::idle_compact`). The user only. Returns `{thread, thread_name, offers}`.
    pub fn compact_offers(
        &self,
        query: &str,
        offers: &str,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user turns a thread's offers to compact its context on or off; tell \
                 the user",
            ));
        }
        let off = match offers {
            "on" => false,
            "off" => true,
            other => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("offers: {other:?} is neither \"on\" nor \"off\""),
                ))
            }
        };
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        core.set_idle_offers(&self.inner, &id, off, caller)
    }
}

impl Core {
    /// Why thread `id` cannot compact its context now (`None`: it can), checked when the user
    /// asks and again when the compaction is due: a thread moved to another runtime meanwhile
    /// (its new runtime's first turn must carry the handoff), parked, attached or gone remote
    /// takes none.
    pub(super) fn compaction_refusal(
        &self,
        hub: &Inner,
        id: &str,
    ) -> Result<Option<String>, RpcError> {
        let view = self.view(id)?;
        let name = &view.name;
        let host = hub.host(&view.box_name);
        if host.as_ref().is_some_and(|h| h.is_remote()) {
            return Ok(Some(format!(
                "{name} runs on box {:?}: twr cannot compact the context of a thread on a \
                 remote box yet",
                view.box_name
            )));
        }
        if !host.is_some_and(|h| h.compacts(&view.runtime)) {
            return Ok(Some(format!(
                "{name}'s runtime {:?} cannot compact its context from twr (claude-code, codex \
                 and grok-build can)",
                view.runtime
            )));
        }
        // (A runtime starting now has a session, though it has not said which yet.)
        let live = self.rt.get(id).is_some_and(|r| r.live.is_some());
        if view.handoff_pending || (view.session_id.is_none() && !live) {
            return Ok(Some(format!(
                "{name} has no session to compact yet: its next turn starts one"
            )));
        }
        if view.parked {
            return Ok(Some(format!("{name} is parked: resume it first")));
        }
        if view.attached.is_some() {
            return Ok(Some(format!(
                "{name} is attached: compact it from its own CLI (/compact), or detach first"
            )));
        }
        Ok(None)
    }

    fn compact_locked(
        &mut self,
        hub: &Arc<Inner>,
        id: &str,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        self.refuse_retired(id)?;
        self.refuse_deleting(id)?;
        self.refuse_switching(id)?;
        if let Some(why) = self.compaction_refusal(hub, id)? {
            return Err(rpc(codes::BAD_REQUEST, why));
        }
        let name = self.thread_name(id);
        let base = json!({"thread": id, "thread_name": name});
        let with = |more: Value| {
            let mut v = base.clone();
            if let (Some(v), Some(more)) = (v.as_object_mut(), more.as_object()) {
                v.extend(more.clone());
            }
            v
        };
        if self.rt.get(id).is_some_and(|r| r.compact.is_some()) {
            return Ok(with(json!({"state": "queued", "already": true})));
        }
        let before = self.view(id)?.turns;
        // An offer card holds the queue (`hub::idle_compact`): this answers it `Compact
        // first`, and the compaction runs before the messages it held.
        if !self.compact_past_idle_offer(hub, id, caller)? {
            let after = self
                .store
                .pending_list(id, usize::MAX)
                .map_err(internal)?
                .last()
                .map_or(0, |p| p.id);
            let view = self.view(id)?;
            let (runtime, session) = (view.runtime.clone(), view.session_id.clone());
            self.rt(id).compact = Some(CompactAsk {
                after,
                source: caller.source(),
                runtime,
                session,
                by: CompactBy::User,
            });
        }
        // The user's own ask, like their send, lets a queue a failure held go (`ThreadRt::
        // queue_held`).
        self.release_queue(id);
        let dispatched = self.try_dispatch(hub, id);
        let waiting = self.rt.get(id).is_some_and(|r| r.compact.is_some());
        match dispatched {
            // Started (its turn is recorded; a runtime that could not take it ended it).
            _ if !waiting => {
                let turn = self.view(id)?.turns;
                if turn > before {
                    Ok(with(json!({"state": "started", "turn": turn})))
                } else {
                    Err(internal("the compaction was dropped before it started"))
                }
            }
            Ok(_) => Ok(with(json!({"state": "queued"}))),
            // Still asked for: the next dispatch tries again.
            Err(e) => {
                tracing::warn!(thread = %id, error = %format!("{e:#}"), "could not start a compaction yet");
                Ok(with(json!({"state": "queued"})))
            }
        }
    }

    /// Whether thread `thread`'s compaction is due ([`Core::dispatch_next`]): one was asked
    /// for, and no message queued before the ask waits (`next`: the next queue row). One that
    /// can no longer run ([`Core::compaction_refusal`]: the thread moved to another runtime
    /// meanwhile, say) is dropped, logged, and the queue goes on.
    pub(super) fn compaction_due(&mut self, hub: &Inner, thread: &str, next: Option<i64>) -> bool {
        let due = self
            .rt
            .get(thread)
            .and_then(|r| r.compact.as_ref())
            .is_some_and(|ask| next.is_none_or(|n| n > ask.after));
        if !due {
            return false;
        }
        // Still the session it was asked for (a session not known then: the runtime it was
        // starting, which is the thread's runtime still).
        let same = match (
            self.rt.get(thread).and_then(|r| r.compact.as_ref()),
            self.registry.get(thread),
        ) {
            (Some(ask), Some(v)) => {
                ask.runtime == v.runtime
                    && ask
                        .session
                        .as_ref()
                        .is_none_or(|s| v.session_id.as_ref() == Some(s))
            }
            _ => false,
        };
        if !same {
            tracing::warn!(
                thread,
                "dropping a compaction asked for: the thread's session changed since"
            );
            self.rt(thread).compact = None;
            return false;
        }
        match self.compaction_refusal(hub, thread) {
            Ok(None) => true,
            Ok(Some(why)) => {
                tracing::warn!(thread, %why, "dropping a compaction asked for: it can no longer run");
                self.rt(thread).compact = None;
                false
            }
            Err(e) => {
                tracing::warn!(thread, error = %e.message, "dropping a compaction asked for");
                self.rt(thread).compact = None;
                false
            }
        }
    }

    /// Starts thread `thread`'s compaction turn ([`Core::compaction_due`]): records its
    /// `turn.started` and sends the runtime [`twr_core::adapter::AdapterCmd::Compact`],
    /// starting the runtime on its session first when none runs. A runtime that cannot take it
    /// ends the turn `error`, as [`Core::deliver`] does for a prompt. The ask is consumed only
    /// once the turn is recorded: a failed append leaves it asked for.
    pub(super) fn start_compact_turn(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
    ) -> anyhow::Result<u64> {
        let Some(ask) = self.rt.get(thread).and_then(|r| r.compact.clone()) else {
            anyhow::bail!("no compaction was asked for");
        };
        let view = self.view(thread).map_err(anyhow::Error::new)?.clone();
        let turn = view.turns + 1;
        let mut twr = Map::new();
        twr.insert(COMPACT_TURN_KEY.into(), json!(true));
        if let Some(by) = ask.by.recorded() {
            twr.insert(COMPACT_BY_KEY.into(), json!(by));
        }
        let mut batch = Batch::default();
        batch.push(
            thread,
            Some(turn),
            ask.source,
            EventBody::TurnStarted(TurnStarted {
                prompt: "/compact".into(),
                from: TurnFrom::User,
                ext: twr_ext_map(Value::Object(twr)),
                ..Default::default()
            }),
        );
        self.append_batch(hub, batch)?;
        self.rt(thread).compact = None;
        let sent = match self.rt.get(thread).and_then(|r| r.live.as_ref()) {
            Some(live) => live
                .handle
                .compact(turn)
                .map_err(|_| anyhow::anyhow!("the runtime exited before it received the turn")),
            None => self.start_runtime(hub, thread, None).and_then(|()| {
                let live = self.rt.get(thread).and_then(|r| r.live.as_ref());
                live.ok_or_else(|| anyhow::anyhow!("the runtime did not start"))?
                    .handle
                    .compact(turn)
                    .map_err(|_| anyhow::anyhow!("the runtime exited before it received the turn"))
            }),
        };
        if let Err(e) = sent {
            // As a prompt that cannot be delivered: the queue waits behind the failure.
            if self.rt.get(thread).is_none_or(|r| r.live.is_none()) {
                self.rt(thread).queue_held = true;
            }
            self.fail_turn(hub, thread, turn, view.session_id.clone(), &e)?;
        }
        Ok(turn)
    }

    /// The hub's reaction to the end of compaction turn `turn` of `thread` (`hub::reactions`):
    /// maintenance, so no grade, report or review (what the last work turn reported stays the
    /// thread's report). A failed one raises an `error` item saying why, once; nothing retries
    /// it.
    pub(super) fn react_to_compaction_end(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        result: TurnResult,
    ) -> anyhow::Result<()> {
        // Failed: the turn did, or its compaction said so (a runtime that ends the turn
        // `completed` all the same, Grok Build's `auto_compact_failed`).
        let compaction_error = match self.stored_event(thread, Some(turn), "context.compacted")? {
            Some(Event {
                body: EventBody::ContextCompacted(c),
                ..
            }) => c.error,
            _ => None,
        };
        if result != TurnResult::Error && compaction_error.is_none() {
            return Ok(());
        }
        let done = crate::store::turn_reactions(self.store.conn(), thread, turn)?;
        if done.errored {
            return Ok(());
        }
        let name = self.thread_name(thread);
        let detail = match self.stored_event(thread, Some(turn), "turn.ended")? {
            Some(Event {
                body: EventBody::TurnEnded(p),
                ..
            }) => super::retry::failure_detail(&p),
            _ => None,
        }
        .or(compaction_error);
        let summary = match detail {
            Some(d) => format!("{name}: compacting its context failed: {d}"),
            None => format!("{name}: compacting its context failed"),
        };
        let mut batch = Batch::default();
        batch.push(
            thread,
            Some(turn),
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: new_id(),
                reason: AttentionReason::Error,
                summary,
                options: vec!["dismiss".into()],
                ..Default::default()
            }),
        );
        self.append_batch(hub, batch)?;
        Ok(())
    }

    /// Whether turn `turn` of `thread` is a compaction the user asked for (its `turn.started`).
    pub(super) fn is_compaction(&self, thread: &str, turn: u64) -> anyhow::Result<bool> {
        Ok(matches!(
            self.stored_event(thread, Some(turn), "turn.started")?,
            Some(Event { body: EventBody::TurnStarted(p), .. }) if is_compact_turn(&p)
        ))
    }
}
