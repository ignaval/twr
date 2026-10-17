//! Adapter supervision: the task that pumps a runtime's events into the hub ([`supervise`]),
//! the checks an adapter event passes before it is recorded ([`Core::record_adapter_event`],
//! [`adapter_may_emit`], attention ids namespaced per thread and runtime), the store retries
//! ([`persist`]), and what happens when recording fails for good ([`give_up_runtime`],
//! [`fail_hub`]).

use std::sync::{Arc, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use twr_core::adapter::{adapter_attn_id, hub_attn_id, AdapterEvent};
use twr_core::blob::strip_body_capping_metadata;
use twr_core::event::*;

use super::append::rejection;
use super::{
    git, AfterReap, Batch, BatchEvent, Core, EndedTurn, HubState, Inner, PendingExit, LOCAL_BOX,
};
use crate::host::StaleProcess;
use crate::store;

impl Core {
    /// Checks and records one adapter event. Returns the turn it ended, if it was an accepted
    /// `turn.ended` (the caller then runs [`Core::react_to_adapter_turn_end`]).
    ///
    /// An event the hub refuses because of its content ([`rejection`]) is dropped and counted
    /// (a refused `turn.ended` is recorded bare). Any other failure (the store) leaves the hub
    /// state untouched, so the caller can retry the same event.
    ///
    /// `ack`: the runner frame the event came in (remote boxes); its watermark is committed
    /// with the event, or alone when the event is dropped, so a frame is recorded exactly once.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_adapter_event(
        &mut self,
        hub: &Inner,
        thread: &str,
        generation: u64,
        source: &Source,
        ev: (EventBody, Option<String>, Option<u64>),
        ack: Option<&store::RunnerAck>,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Option<EndedTurn>> {
        let current = self
            .rt
            .get(thread)
            .and_then(|r| r.live.as_ref())
            .map(|l| (l.generation, l.given_up.is_some()));
        match current {
            Some((g, false)) if g == generation => {}
            Some((g, true)) if g == generation => {
                // The hub lost part of this runtime's output: nothing more of it is recorded
                // (its exit records the crash).
                tracing::debug!(thread, "dropping event from a runtime the hub gave up on");
                self.commit_ack(hub, ack)?;
                return Ok(None);
            }
            _ => {
                tracing::warn!(
                    thread,
                    "dropping event from a runtime the hub already closed"
                );
                self.commit_ack(hub, ack)?;
                return Ok(None);
            }
        }
        let (mut body, raw, turn) = ev;
        // Usage reports are folded into `account.usage`, never recorded on the thread
        // (`hub::usage`).
        if let EventBody::RuntimeLimits(limits) = body {
            return self.record_usage(hub, thread, limits, ack, at);
        }
        // The turn is the producer's (a runner's frame): one SQLite cannot hold would fail the
        // store, and so the frame, on every replay (the connection would never get past it).
        if turn.is_some_and(|t| i64::try_from(t).is_err()) {
            tracing::warn!(
                thread,
                kind = body.kind(),
                ?turn,
                "dropping an adapter event whose turn is out of range"
            );
            self.commit_ack(hub, ack)?;
            self.dropped_events += 1;
            return Ok(None);
        }
        if !adapter_may_emit(&body) {
            tracing::warn!(
                thread,
                kind = body.kind(),
                "dropping an event kind adapters may not emit"
            );
            self.commit_ack(hub, ack)?;
            return Ok(None);
        }
        // Capping metadata is the hub's alone: a producer's `_capped` marker or `<field>_ref`
        // sibling would make replay hydrate something the live state never saw.
        match strip_body_capping_metadata(&body) {
            Ok(None) => {}
            Ok(Some((clean, removed))) => {
                tracing::warn!(
                    thread,
                    kind = body.kind(),
                    removed,
                    "stripped capping metadata an adapter supplied"
                );
                self.sanitized_events += 1;
                body = clean;
            }
            Err(e) => {
                tracing::warn!(thread, kind = body.kind(), error = %e, "dropping an adapter event that does not reserialize");
                self.commit_ack(hub, ack)?;
                self.dropped_events += 1;
                return Ok(None);
            }
        }
        namespace_attention(thread, self.runtime_gen(thread), &mut body);
        if let EventBody::AttentionResolved(p) = &body {
            if !self.is_adapter_withdrawal(thread, p) {
                tracing::warn!(
                    thread,
                    attn_id = %p.attn_id,
                    "dropping attention.resolved for an item this runtime did not raise or that is closed"
                );
                self.commit_ack(hub, ack)?;
                return Ok(None);
            }
        }
        // Attention ids are global, and an adapter's are in its thread's namespace
        // (`namespace_attention`), so no other thread's item or hub item can carry one; this
        // stays as a safety net. Within the namespace, re-raising an open id updates that item
        // as a new revision (`AttentionStack`; adapters never do, see `Adapter`: a replaced
        // request comes as a withdrawal plus a new id, so this covers replays and scripted
        // adapters), and a resolved id may come back: runtimes number their requests per
        // process, so a resumed one reuses ids.
        if let EventBody::AttentionRaised(p) = &body {
            let taken = self.attention.get(&p.attn_id).is_some_and(|item| {
                item.thread != thread || !self.adapter_attn.contains_key(&p.attn_id)
            });
            if taken {
                tracing::warn!(
                    thread,
                    attn_id = %p.attn_id,
                    "dropping attention.raised whose id is another thread's or the hub's open item"
                );
                self.commit_ack(hub, ack)?;
                self.dropped_events += 1;
                return Ok(None);
            }
        }
        let open = self.registry.get(thread).and_then(|v| v.current_turn);
        let open_runtime_turn = self
            .registry
            .get(thread)
            .and_then(|v| v.current_runtime_turn.clone());
        // The hub alone numbers turns. A runtime's own turn comes without a number and takes
        // the next one when no turn is open. One that raced a turn the hub started (its prompt
        // on its way to the runtime) is not recorded: its events, which carry no turn, go to
        // the open turn, and its end, which carries its id (`ext.twr.runtime_turn`), closes
        // only the turn its start opened. The decision follows from the log alone, so a
        // runner's replay meets the same one. The usage of a turn whose start was dropped goes
        // to the end of the turn that took it in, once: recorded only after the frame's ack
        // commits (a failed one retries the frame), and only for that turn while it is open.
        let runtime_end = match &body {
            EventBody::TurnEnded(p) => runtime_turn(p.ext.as_ref()).map(str::to_string),
            _ => None,
        };
        let turn = if let EventBody::TurnStarted(p) = &body {
            if let Some(t) = open {
                tracing::debug!(
                    thread,
                    ?open,
                    "dropping a runtime's turn.started: a turn is open"
                );
                let id = runtime_turn(p.ext.as_ref()).map(str::to_string);
                self.commit_ack(hub, ack)?;
                if let Some(id) = id {
                    self.rt(thread).absorbed.insert(id, t);
                }
                return Ok(None);
            }
            Some(self.registry.get(thread).map_or(1, |v| v.turns + 1))
        } else if matches!(body, EventBody::TaskUpdated(_)) {
            None
        } else if let Some(id) = runtime_end {
            if open_runtime_turn.as_deref() != Some(id.as_str()) {
                tracing::debug!(
                    thread,
                    runtime_turn = %id,
                    ?open,
                    "dropping the end of a runtime turn that is not the open one"
                );
                let usage = match &body {
                    EventBody::TurnEnded(p) => p.usage.clone(),
                    _ => None,
                };
                self.commit_ack(hub, ack)?;
                let rt = self.rt(thread);
                let into = rt.absorbed.remove(&id).filter(|t| Some(*t) == open);
                if let (Some(t), Some(u)) = (into, usage) {
                    rt.folded_usage = Some(match rt.folded_usage.take() {
                        Some((at, carried)) if at == t => (t, carried.add(&u)),
                        _ => (t, u),
                    });
                }
                return Ok(None);
            }
            open
        } else if session_scoped(&body) {
            turn
        } else if in_turn_kind(&body) {
            turn.or(open)
        } else {
            turn
        };
        let ended = match &body {
            EventBody::TurnEnded(p) => {
                if open.is_none() || turn != open {
                    tracing::warn!(
                        thread,
                        ?turn,
                        ?open,
                        "dropping turn.ended for a turn that is not open"
                    );
                    self.commit_ack(hub, ack)?;
                    return Ok(None);
                }
                Some(p.result)
            }
            _ => None,
        };
        // A turn whose message the user took back (`hub::withdraw`) ends `interrupted`,
        // whatever its runtime reports (`ext.twr.runtime_result` keeps that): like any
        // interrupted turn, it gets no grade, report, review or retry.
        let withdrawn = ended.is_some() && turn.is_some_and(|t| self.turn_taken_back(thread, t));
        let ended = match (&mut body, withdrawn) {
            (EventBody::TurnEnded(p), true) => {
                if p.result != TurnResult::Interrupted {
                    let was = serde_json::to_value(p.result).unwrap_or_default();
                    let ext = p.ext.get_or_insert_with(JsonMap::new);
                    let twr = ext.entry("twr").or_insert_with(|| serde_json::json!({}));
                    if !twr.is_object() {
                        *twr = serde_json::json!({});
                    }
                    if let Some(t) = twr.as_object_mut() {
                        t.insert("runtime_result".into(), was);
                    }
                    p.result = TurnResult::Interrupted;
                }
                Some(TurnResult::Interrupted)
            }
            _ => ended,
        };
        // Kept until this end is recorded: a store failure retries the event.
        if let (Some(t), EventBody::TurnEnded(p)) = (ended.and(turn), &mut body) {
            let carried = self.rt.get(thread).and_then(|r| r.folded_usage.clone());
            if let Some((_, u)) = carried.filter(|(at, _)| *at == t) {
                p.usage = Some(
                    p.usage
                        .as_ref()
                        .map_or_else(|| u.clone(), |own| own.add(&u)),
                );
            }
        }
        // (`ext.twr.start_time` of a local runtime was stamped off the core lock, by
        // `supervise`: `crate::host::stamp_start_time`.) The permission mode, model and effort
        // the hub started the runtime with go with its start (`hub::restart`), so the log says
        // what it runs in.
        // A runtime the hub took over after a restart (`hub::runners`) has no settings
        // recorded here: its runner stamped the ones it started it with. What neither says (a
        // runner from before stamping, or before efforts) is unknown, never the thread's
        // setting now (it may have changed since the start): recorded as unknown, which a
        // settings restart replaces.
        if let EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p) = &mut body {
            use super::restart::{stamp, stamped, RunSettings};
            let settings = self
                .rt
                .get(thread)
                .and_then(|r| r.runtime_settings.clone())
                .or_else(|| stamped(p, source))
                .unwrap_or_else(RunSettings::unknown);
            stamp(p, &settings);
        }
        let fallback = match &body {
            // A refused turn.ended would leave the turn open forever: record the bare result.
            EventBody::TurnEnded(p) => Some(EventBody::TurnEnded(TurnEnded {
                result: p.result,
                ..Default::default()
            })),
            _ => None,
        };
        let one = |body: EventBody, raw: Option<String>| {
            let mut batch = Batch {
                runner_ack: ack.cloned(),
                at,
                ..Batch::default()
            };
            batch.events.push(BatchEvent {
                thread: thread.to_string(),
                turn,
                source: source.clone(),
                body,
                raw,
            });
            batch
        };
        let running = super::withdraw::RunningSign::of(&body);
        if let Err(e) = self.append_batch(hub, one(body, raw)) {
            let Some(why) = rejection(&e) else {
                return Err(e);
            };
            tracing::warn!(thread, error = %why, "dropping an invalid adapter event");
            match fallback {
                Some(bare) => {
                    self.append_batch(hub, one(bare, None))?;
                    self.dropped_events += 1;
                }
                None => {
                    self.commit_ack(hub, ack)?;
                    self.dropped_events += 1;
                    return Ok(None);
                }
            }
        }
        // Recorded: what follows is no reason to record the event again.
        if let (Some(sign), Some(t)) = (running, turn) {
            self.withdrawn_turn_runs(hub, thread, t, sign);
        }
        if let (Some(_), Some(t)) = (ended, turn) {
            if let Some(rt) = self.rt.get_mut(thread) {
                rt.folded_usage = None;
                rt.absorbed.retain(|_, into| *into != t);
            }
        }
        Ok(match (ended, turn) {
            (Some(result), Some(turn)) => Some(EndedTurn {
                turn,
                runtime_gen: self.runtime_gen(thread),
                result,
                final_text: self.rt.get_mut(thread).and_then(|r| r.final_text.take()),
            }),
            _ => None,
        })
    }
}

/// Moves an adapter's attention id (`attention.raised`, or a withdrawal) into its thread's
/// namespace and its runtime's generation `gen` ([`Core::runtime_gen`]):
/// `<thread>:<gen>:<adapter id>` ([`runtime_attn_id`]). Runtimes number their requests per
/// process, so the same id comes from several threads (and boxes), and from the thread's next
/// runtime: an answer given to an item of an earlier runtime never reaches a later one.
pub(super) fn namespace_attention(thread: &str, gen: u64, body: &mut EventBody) {
    match body {
        EventBody::AttentionRaised(p) => p.attn_id = runtime_attn_id(thread, gen, &p.attn_id),
        EventBody::AttentionResolved(p) => p.attn_id = runtime_attn_id(thread, gen, &p.attn_id),
        _ => {}
    }
}

/// The hub's id of attention item `adapter_id` of runtime generation `gen` of `thread`:
/// `<thread>:<gen>:<adapter id>`.
pub fn runtime_attn_id(thread: &str, gen: u64, adapter_id: &str) -> String {
    hub_attn_id(thread, &format!("{gen}:{adapter_id}"))
}

/// The runtime generation and the runtime's own id of `hub_id`, an item of `thread`
/// ([`runtime_attn_id`]); the generation is `None` for an id namespaced before generations
/// (`<thread>:<adapter id>`). `None` when `hub_id` is not in the thread's namespace (a
/// hub-raised item).
pub(super) fn split_runtime_attn_id<'a>(
    thread: &str,
    hub_id: &'a str,
) -> Option<(Option<u64>, &'a str)> {
    let rest = adapter_attn_id(thread, hub_id)?;
    Some(match rest.split_once(':') {
        Some((g, id)) if !g.is_empty() && g.bytes().all(|b| b.is_ascii_digit()) => {
            match g.parse() {
                Ok(g) => (Some(g), id),
                Err(_) => (None, rest),
            }
        }
        _ => (None, rest),
    })
}

/// Kinds an adapter may emit, including the `turn.started` of a turn the runtime started
/// itself (`from: runtime`, numbered by the hub). Everything else (`report.*`, `decision.*`,
/// any other `turn.started`, `thread.created`, ...) is the hub's or the user's to write.
///
/// `attention.resolved` passes only as a withdrawal (`by: timeout`, no answer): a runtime may
/// cancel its own request but never answer one (relay rule). The hub then also checks that the
/// item is open and was raised by the thread's runtime.
///
/// `thread.stopped` / `thread.crashed` are the supervisor's: an adapter reports its process
/// ending with `AdapterEvent::Exited`, and the hub then runs the closing sequence (turn
/// interrupted, adapter attention resolved) before appending them. Letting an adapter append
/// `thread.stopped` itself would clear the open turn and attention in the reducers first and
/// skip those closing events.
pub fn adapter_may_emit(body: &EventBody) -> bool {
    matches!(
        body,
        EventBody::ThreadStarted(_)
            | EventBody::ThreadResumed(_)
            | EventBody::AgentText(_)
            | EventBody::AgentToolCall(_)
            | EventBody::AgentToolResult(_)
            | EventBody::AgentThinking(_)
            | EventBody::AttentionRaised(_)
            | EventBody::TurnEnded(_)
            | EventBody::TaskUpdated(_)
    ) || matches!(body, EventBody::TurnStarted(p) if p.from == TurnFrom::Runtime)
        || matches!(
            body,
            EventBody::AttentionResolved(p) if p.by == ResolvedBy::Timeout && p.answer.is_none()
        )
}

/// An `attention.raised` for a request of the runtime's session rather than of its turn
/// (`ext.twr.session`): recorded outside any turn, so the end of the open turn leaves it open.
fn session_scoped(body: &EventBody) -> bool {
    matches!(body, EventBody::AttentionRaised(p) if p
        .ext
        .as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(SESSION_SCOPE_KEY))
        .and_then(serde_json::Value::as_bool)
        == Some(true))
}

/// Kinds that belong to a turn; an adapter event of these kinds without a turn gets the open
/// one.
fn in_turn_kind(body: &EventBody) -> bool {
    matches!(
        body,
        EventBody::AgentText(_)
            | EventBody::AgentToolCall(_)
            | EventBody::AgentToolResult(_)
            | EventBody::AgentThinking(_)
            | EventBody::AttentionRaised(_)
            | EventBody::AttentionResolved(_)
            | EventBody::TurnEnded(_)
    )
}

/// Pumps one adapter's events into the hub until it exits.
///
/// Events are recorded strictly in order: when the store fails, the same event is retried
/// ([`persist`]) before the next one is taken from the adapter. If it still fails, the hub
/// stops this runtime and records its closing sequence ([`give_up_runtime`]); when even that
/// cannot be recorded the hub fails ([`fail_hub`]) instead of running on with state its log
/// does not have.
pub(super) async fn supervise(
    hub: Arc<Inner>,
    thread: String,
    generation: u64,
    source: Source,
    mut events: mpsc::UnboundedReceiver<AdapterEvent>,
) {
    loop {
        let (code, stderr_tail) = match events.recv().await {
            Some(AdapterEvent::Emit {
                mut body,
                raw,
                turn,
            }) => {
                // The runtime's start time is read before the core lock is taken.
                if matches!(
                    body,
                    EventBody::ThreadStarted(_) | EventBody::ThreadResumed(_)
                ) {
                    let host =
                        (hub.core().registry.get(&thread)).and_then(|v| hub.host(&v.box_name));
                    crate::host::stamp_start_time(host, &thread, &mut body).await;
                }
                let ev = (body, raw, turn);
                let recorded = persist(&hub, &thread, "recording an adapter event", |core| {
                    core.record_adapter_event(
                        &hub,
                        &thread,
                        generation,
                        &source,
                        ev.clone(),
                        None,
                        None,
                    )
                })
                .await;
                let ended = match recorded {
                    Ok(ended) => ended,
                    Err(e) => {
                        give_up_runtime(&hub, &thread, generation, e).await;
                        continue;
                    }
                };
                if let Some(t) = ended {
                    let reacted = persist(&hub, &thread, "reacting to the end of a turn", |core| {
                        core.react_to_adapter_turn_end(&hub, &thread, &t)
                    })
                    .await;
                    if let Err(e) = reacted {
                        give_up_runtime(&hub, &thread, generation, e).await;
                    }
                    git::snapshot_soon(&hub, &thread);
                }
                continue;
            }
            Some(AdapterEvent::Exited { code, stderr_tail }) => (code, stderr_tail),
            None => {
                // The adapter went away without reporting an exit: its process may still run.
                // It is killed first (identity-checked), and the exit recorded only once none
                // is left; until then the runtime keeps its slot.
                while reap_runtime(&hub, &thread, generation).await == Some(false) {
                    tracing::error!(%thread, "the runtime of an adapter that went away still runs after being killed; retrying");
                    tokio::time::sleep(hub.stop_timeout).await;
                }
                (
                    None,
                    "adapter event channel closed without an exit".to_string(),
                )
            }
        };
        let pending = hub
            .core()
            .begin_exit(&thread, generation, code, stderr_tail);
        if let Some(mut exit) = pending {
            close_exited(&hub, &thread, &mut exit).await;
        }
        return;
    }
}

/// What [`close_exited`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Closed {
    /// The closing sequence (and the runner frame it came in, if any) was recorded.
    Recorded,
    /// The exit's connection proved a token the box no longer has (`box.token` changed it
    /// before the commit, and put the runtime back as it was then): nothing was recorded.
    Revoked,
    /// The store failed for good (the hub fails).
    Failed,
}

/// Records the closing sequence of an exited runtime (retrying the store), releases it, and
/// dispatches the queue after a requested stop.
///
/// An exit a runner connection brought (its `exited` frame, or a runtime its reconciliation
/// found lost) commits nothing once `box.token` changed or revoked the token the connection
/// proved, checked under the core lock at each attempt ([`Closed::Revoked`]). The change
/// itself put the runtime the exit took back, live and not exiting, in its own step under the
/// core lock ([`Core::put_back_revoked_exits`]): a connection with the box's token that comes
/// before this attempt finds the runtime as it was, and reports or finds the exit again. A
/// connection fenced by a newer one of the box records the closing sequence without its
/// frame's watermark (the frame comes again, and finds the runtime closed).
///
/// The commit and the exit's bookkeeping ([`Core::finish_exit`]: the runtime no longer
/// `exiting`) are one step under the core lock: a `thread.stop` never sees the runtime closed
/// by the log but still exiting. (One that did would record a durable stop request that no
/// exit completes: after a mode restart's `thread.stopped` the thread then stayed not stopped,
/// with no runtime, and every later prompt stayed queued.) A stop that comes
/// before the commit is the exit's own ([`Core::exit_plan`] closes it as stopped); one that
/// comes after finds no runtime and closes the thread itself.
pub(super) async fn close_exited(hub: &Arc<Inner>, thread: &str, exit: &mut PendingExit) -> Closed {
    let mut dispatch = None;
    let mut revoked = false;
    let closed = persist(hub, thread, "closing an exited runtime", |core| {
        // Revoked: the runtime is back already, and may be another connection's exit by now.
        if exit.fence.as_ref().is_some_and(|f| f.revoked()) {
            revoked = true;
            return Ok(());
        }
        let plan = core.exit_plan(thread, exit);
        // A runner connection fenced meanwhile commits no watermark (its frame is replayed).
        let ack = match &exit.fence {
            Some(f) if !f.holds() => None,
            _ => exit.runner_ack.clone(),
        };
        let recorded = plan.is_some();
        if !recorded {
            tracing::warn!(
                thread,
                "an exit of a runtime the log has already closed: recording nothing more"
            );
        }
        let mut plan = plan.unwrap_or_default();
        core.run_plan_at(hub, thread, &mut plan, ack.as_ref(), exit.at)?;
        // An exit that recorded nothing (its runtime was closed already) is no exit of the
        // thread's runtime: nothing waits for it and nothing follows it.
        if !recorded {
            exit.crash = false;
            exit.stopped = false;
            exit.restart = false;
        }
        dispatch = Some(core.finish_exit(hub, thread, exit, recorded));
        Ok(())
    })
    .await;
    #[cfg(test)]
    tests::after_close_commit();
    if revoked {
        tracing::info!(
            thread,
            "an exit a connection brought whose token the box no longer has: nothing recorded"
        );
        return Closed::Revoked;
    }
    let dispatch = match dispatch {
        Some(d) => d,
        // Never committed: released all the same (the hub fails below), unless a token change
        // put the runtime back meanwhile (it keeps its slot).
        None => {
            let mut core = hub.core();
            !exit.fence.as_ref().is_some_and(|f| f.revoked())
                && core.finish_exit(hub, thread, exit, false)
        }
    };
    if let Err(e) = closed {
        fail_hub(
            hub,
            format!("cannot record the closing sequence of thread {thread}: {e:#}"),
        );
        return Closed::Failed;
    }
    if exit.crash {
        // A crash: classify it and retry once if it is due (`hub::retry`); nothing else is
        // dispatched.
        let r = persist(hub, thread, "reacting to a crash", |core| {
            core.react_to_crash(hub, thread)
        })
        .await;
        if let Err(e) = r {
            tracing::error!(thread, error = %format!("{e:#}"), "cannot react to the crash");
        }
    }
    if dispatch {
        let r = persist(hub, thread, "dispatching the queue", |core| {
            core.try_dispatch(hub, thread).map(|_| ())
        })
        .await;
        if let Err(e) = r {
            fail_hub(
                hub,
                format!("cannot dispatch the queued prompts of thread {thread}: {e:#}"),
            );
        }
    }
    // Stopped for a settings restart and nothing queued started it: start it again now, on
    // its session, in its new settings (`hub::restart`).
    if dispatch && exit.restart {
        hub.core().restart_after_settings_change(hub, thread);
    }
    Closed::Recorded
}

/// Recording what runtime `generation` of `thread` did failed for good (`e`): stops the
/// runtime, which "crashed" (the hub lost part of its output). Its later events are dropped;
/// its closing sequence (the crash) is recorded when its exit is observed, like any exit, and
/// until then it keeps its slot: the thread takes no turn and is neither attached nor parked,
/// so no replacement ever runs next to it in the same session and worktree. A local runtime
/// that outlives the stop timeout is killed ([`kill_given_up`]). When the runtime is already
/// closed the hub fails ([`fail_hub`]): it cannot record what the runtime did.
pub(super) async fn give_up_runtime(
    hub: &Arc<Inner>,
    thread: &str,
    generation: u64,
    e: anyhow::Error,
) {
    let why = format!("{e:#}");
    tracing::error!(thread, error = %why, "cannot record the runtime's events; stopping it");
    let first = {
        let mut core = hub.core();
        let live = core
            .rt
            .get_mut(thread)
            .and_then(|r| r.live.as_mut())
            .filter(|l| l.generation == generation);
        live.map(|live| {
            let first = live.given_up.is_none();
            if first {
                live.given_up = Some(format!(
                    "twr: the hub could not record this runtime's events ({why}) and stopped it"
                ));
            }
            let _ = live.handle.stop();
            first
        })
    };
    match first {
        Some(true) => {
            tokio::spawn(kill_given_up(
                Arc::clone(hub),
                thread.to_string(),
                generation,
            ));
        }
        Some(false) => {}
        // The runtime is already closed, yet the hub could not record what it did.
        None => fail_hub(
            hub,
            format!("cannot record the events of thread {thread}: {why}"),
        ),
    }
}

/// A runtime the hub gave up on ([`give_up_runtime`]) that is still there after the stop
/// timeout is killed (a local one; identity-checked and scoped to its generation,
/// [`reap_runtime`]). Its adapter then reports the exit, which records the crash. Nothing is
/// recorded here.
async fn kill_given_up(hub: Arc<Inner>, thread: String, generation: u64) {
    tokio::time::sleep(hub.stop_timeout).await;
    match reap_runtime(&hub, &thread, generation).await {
        // Gone (its exit closed it) or killed.
        None | Some(true) => {}
        Some(false) => {
            tracing::error!(%thread, "a runtime the hub gave up on still runs after being killed; the thread waits for its exit");
        }
    }
}

impl Core {
    /// The end of a kill [`reap_runtime`] counted in `thread`'s [`ThreadRt::reaping`]: what the
    /// last one runs ([`AfterReap`]). The thread may be gone by now (a deletion waits for the
    /// kill, but nothing counts on it here): nothing is recreated for it, and the count never
    /// goes below zero.
    ///
    /// [`ThreadRt::reaping`]: super::ThreadRt::reaping
    pub(super) fn reaped(&mut self, thread: &str) -> AfterReap {
        let Some(rt) = self.rt.get_mut(thread) else {
            return AfterReap::Nothing;
        };
        rt.reaping = rt.reaping.saturating_sub(1);
        if rt.reaping == 0 {
            std::mem::take(&mut rt.after_reap)
        } else {
            AfterReap::Nothing
        }
    }
}

/// Kills what may be left of runtime `generation` of `thread` on the local box
/// (identity-checked: the recorded process, and the processes marked for the thread and this
/// generation since this hub started) and returns whether none is left running
/// ([`ThreadHost::runtime_running`]). `None`: that runtime is not the thread's live one any
/// more (its exit was recorded), nothing was done. A remote box's runner reports its runtimes'
/// exits itself: `Some(true)`.
///
/// Until the kill is done the thread counts as busy ([`ThreadRt::reaping`]): its exit may be
/// recorded meanwhile, and no runtime starts next to a kill in progress (nor is the thread
/// attached or parked, and a deletion waits for it, `hub::delete`). A dispatch held back by it
/// runs at the end ([`AfterReap`], [`Core::reaped`]).
///
/// [`ThreadHost::runtime_running`]: crate::host::ThreadHost::runtime_running
/// [`ThreadRt::reaping`]: super::ThreadRt::reaping
pub(super) async fn reap_runtime(hub: &Arc<Inner>, thread: &str, generation: u64) -> Option<bool> {
    let (host, proc) = {
        let mut core = hub.core();
        let ours = core
            .rt
            .get(thread)
            .and_then(|r| r.live.as_ref())
            .is_some_and(|l| l.generation == generation);
        if !ours {
            return None;
        }
        let box_name = core
            .registry
            .get(thread)
            .map_or_else(|| LOCAL_BOX.to_string(), |v| v.box_name.clone());
        let Some(host) = hub.host(&box_name).filter(|h| !h.is_remote()) else {
            return Some(true);
        };
        // Recorded for the live runtime, i.e. this generation's process (and signalled only
        // while it still carries this generation's marker).
        let rt = core.rt.get_mut(thread)?;
        let proc = rt.pid.map(|pid| StaleProcess {
            thread: thread.to_string(),
            pid,
            start_time: rt.start_time,
            exe: rt.exe.clone(),
            generation: Some(generation.to_string()),
        });
        // Checked with the generation, under the same lock: nothing can start in between.
        rt.reaping += 1;
        (host, proc)
    };
    let (t, min_start) = (thread.to_string(), hub.start_ticks);
    let gone = tokio::task::spawn_blocking(move || {
        if let Some(p) = &proc {
            host.reap_stale(p);
        }
        if let Some(m) = min_start {
            host.reap_marked(&t, m, Some(generation));
        }
        !host.runtime_running(&t, proc.as_ref(), min_start, Some(generation))
    })
    .await;
    let gone = match gone {
        Ok(gone) => gone,
        Err(e) => {
            tracing::error!(error = %e, "killing a runtime failed");
            false
        }
    };
    let after = hub.core().reaped(thread);
    let what = match after {
        AfterReap::Nothing => None,
        AfterReap::Retry => Some("retrying after a runtime was killed"),
        AfterReap::Dispatch => Some("dispatching after a runtime was killed"),
    };
    if let Some(what) = what {
        let r = persist(hub, thread, what, |core| match after {
            AfterReap::Retry => core.start_retry(hub, thread).map(|_| ()),
            _ => core.try_dispatch(hub, thread).map(|_| ()),
        })
        .await;
        if let Err(e) = r {
            fail_hub(
                hub,
                format!("cannot dispatch the queued prompts of thread {thread}: {e:#}"),
            );
        }
    }
    Some(gone)
}

/// Delays between attempts at a store write that failed (5 retries, about 2 s in all).
const PERSIST_RETRY: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(150),
    Duration::from_millis(300),
    Duration::from_millis(600),
    Duration::from_millis(900),
];

/// Runs `f` under the core lock, retrying failures with backoff ([`PERSIST_RETRY`]; the lock
/// is not held while waiting). `f` must leave the hub unchanged when it fails, or be safe to
/// repeat. Once the hub has failed ([`fail_hub`]) there is a single attempt.
pub(super) async fn persist<T>(
    hub: &Inner,
    thread: &str,
    what: &str,
    mut f: impl FnMut(&mut Core) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut delays = PERSIST_RETRY.iter();
    loop {
        let r = f(&mut hub.core());
        let e = match r {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let delay = match delays.next() {
            Some(d) if hub.fatal().is_none() => *d,
            _ => return Err(e),
        };
        hub.core().persist_retries += 1;
        tracing::warn!(thread, error = %format!("{e:#}"), ?delay, "{what} failed; retrying");
        tokio::time::sleep(delay).await;
    }
}

/// The store cannot record what the hub must record: stops serving (the server shuts the hub
/// down and exits with this error) instead of running on with state the log does not have.
pub(super) fn fail_hub(hub: &Inner, msg: String) {
    tracing::error!(error = %msg, "fatal: the hub cannot record its state; shutting down");
    {
        let mut fatal = hub.fatal.lock().unwrap_or_else(PoisonError::into_inner);
        if fatal.is_none() {
            *fatal = Some(msg);
        }
    }
    hub.state.send_if_modified(|s| {
        if *s == HubState::Running {
            *s = HubState::ShutdownRequested;
            true
        } else {
            false
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::adapter::AdapterHandle;
    use twr_core::config::Config;

    use super::super::{Batch, Caller, Hub, HubOptions, Live, NewThread};

    thread_local! {
        /// Runs once, right after [`close_exited`] committed a closing sequence and released
        /// the core lock.
        static AFTER_CLOSE_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            std::cell::RefCell::new(None);
    }

    /// The hook [`close_exited`] calls once its commit's lock is released (see
    /// [`AFTER_CLOSE_COMMIT`]).
    pub(super) fn after_close_commit() {
        if let Some(f) = AFTER_CLOSE_COMMIT.with(|h| h.borrow_mut().take()) {
            f();
        }
    }

    fn fake_hub_options(dir: &std::path::Path) -> HubOptions {
        let mut opts = HubOptions::new(dir.join("twr.db"), Config::default());
        opts.adapters = Arc::new(
            |name: &str| -> Option<Arc<dyn twr_core::adapter::Adapter>> {
                (name == "fake").then(|| {
                    Arc::new(twr_adapters::FakeAdapter::new())
                        as Arc<dyn twr_core::adapter::Adapter>
                })
            },
        );
        opts
    }

    /// Waits (at most 10 s) until `pred` holds of the core.
    async fn wait_core(inner: &Inner, pred: impl Fn(&Core) -> bool) {
        for _ in 0..1000 {
            if pred(&inner.core()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out");
    }

    /// A user's `thread.stop` that comes right after a mode
    /// restart's closing sequence (`thread.stopped`, reason `mode_change`) was committed finds
    /// the runtime closed, not still exiting: it stops the thread then and there (no durable
    /// stop request left that no exit would complete), and the next prompt runs. Before, the
    /// stop answered `stopping: true`, the thread stayed not stopped with no runtime, and the
    /// next prompt stayed queued forever.
    #[tokio::test]
    async fn a_stop_right_after_a_mode_restarts_closing_commit_stops_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(fake_hub_options(dir.path())).await.unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let created = hub
            .new_thread(
                NewThread {
                    project: proj.to_string_lossy().into_owned(),
                    runtime: Some("fake".into()),
                    name: Some("t".into()),
                    ..Default::default()
                },
                "c",
            )
            .await
            .unwrap();
        let t = created["id"].as_str().unwrap().to_string();
        hub.send(&t, "hi", TurnFrom::User, Source::User("c".into()))
            .unwrap();
        let inner = Arc::clone(&hub.inner);
        wait_core(&inner, |core| {
            core.view(&t)
                .is_ok_and(|v| v.current_turn.is_none() && v.session_id.is_some())
                && core.rt.get(&t).is_some_and(|r| r.live.is_some())
        })
        .await;
        // A mode restart as `hub::restart` begins it (durable stop request kept with the
        // queue), and the runtime's exit taken.
        let mut exit = {
            let mut core = inner.core();
            let batch = Batch {
                stop_requests: vec![t.clone()],
                ..Batch::default()
            };
            core.append_batch(&inner, batch).unwrap();
            let rt = core.rt(&t);
            rt.stop_requested = true;
            rt.restart_reason = Some(MODE_RESTART_REASON);
            let live = rt.live.as_mut().unwrap();
            live.stop_requested = true;
            let gen = live.generation;
            core.begin_exit(&t, gen, Some(0), String::new()).unwrap()
        };
        assert!(exit.stopped && exit.restart);
        // The user's stop, right after the closing commit.
        let answer = Arc::new(std::sync::Mutex::new(None));
        let (inner2, t2, answer2) = (Arc::clone(&inner), t.clone(), Arc::clone(&answer));
        AFTER_CLOSE_COMMIT.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let r = inner2
                    .core()
                    .stop_thread(&inner2, &t2, &Caller::user("c"))
                    .unwrap();
                *answer2.lock().unwrap() = Some(r);
            }))
        });
        assert_eq!(close_exited(&inner, &t, &mut exit).await, Closed::Recorded);
        let answer = answer.lock().unwrap().take().expect("the hook ran");
        assert_eq!(answer["stopping"], false, "{answer}");
        {
            let core = inner.core();
            let view = core.view(&t).unwrap();
            assert!(view.stopped, "the thread is stopped");
            let rt = &core.rt[&t];
            assert!(!rt.stop_requested && !rt.exiting && rt.restart_reason.is_none());
            assert!(rt.live.is_none(), "not restarted after the stop");
            assert!(core.store.stop_requests().unwrap().is_empty());
        }
        let evs = hub.thread_events(&t, 0, 10_000).unwrap().1;
        let stops: Vec<bool> = evs
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ThreadStopped(p) => Some(is_settings_restart(p)),
                _ => None,
            })
            .collect();
        assert_eq!(
            stops,
            [true, false],
            "a mode restart's stop, then the user's"
        );
        // The next prompt runs.
        let turns = |hub: &Hub| {
            hub.thread_events(&t, 0, 10_000)
                .unwrap()
                .1
                .iter()
                .filter(|e| e.kind() == "turn.started")
                .count()
        };
        let before = turns(&hub);
        hub.send(&t, "again", TurnFrom::User, Source::User("c".into()))
            .unwrap();
        wait_core(&inner, |core| {
            core.view(&t).is_ok_and(|v| !v.stopped)
                && core.rt.get(&t).is_some_and(|r| r.live.is_some())
        })
        .await;
        assert_eq!(turns(&hub), before + 1);
        hub.shutdown().await;
    }

    /// A closing sequence is planned from the log at each attempt to record it, and records
    /// nothing once the log has closed the runtime that exited: two exits taken for the same
    /// runtime of the log (both planned while it was still open) close it once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_exits_of_one_runtime_of_the_log_close_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = HubOptions::new(dir.path().join("twr.db"), Config::default());
        opts.adapters = Arc::new(
            |name: &str| -> Option<Arc<dyn twr_core::adapter::Adapter>> {
                (name == "fake").then(|| {
                    Arc::new(twr_adapters::FakeAdapter::new())
                        as Arc<dyn twr_core::adapter::Adapter>
                })
            },
        );
        let hub = Hub::open(opts).await.unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let created = hub
            .new_thread(
                NewThread {
                    project: proj.to_string_lossy().into_owned(),
                    runtime: Some("fake".into()),
                    name: Some("t".into()),
                    ..Default::default()
                },
                "c",
            )
            .await
            .unwrap();
        let t = created["id"].as_str().unwrap().to_string();
        hub.send(&t, "!sleep 60000", TurnFrom::User, Source::User("c".into()))
            .unwrap();
        let inner = Arc::clone(&hub.inner);
        let (mut first, mut second) = {
            let mut core = inner.core();
            assert_eq!(core.view(&t).unwrap().current_turn, Some(1));
            let gen = core.rt[&t].live.as_ref().unwrap().generation;
            let first = core.begin_exit(&t, gen, Some(1), "first".into()).unwrap();
            // Another session taken for the same runtime of the log (as a reconnecting runner's
            // took over one being closed), whose exit comes while the first is not recorded.
            let (handle, _end) = AdapterHandle::pair();
            core.generation += 1;
            let gen = core.generation;
            let log_runtime = core.runtime_gen(&t);
            core.rt(&t).live = Some(Live {
                handle,
                generation: gen,
                stop_requested: false,
                attach: false,
                park: false,
                _mcp_file: None,
                _workdir: None,
                run: None,
                run_confirmed: false,
                given_up: None,
                log_runtime,
            });
            inner.live_count.send_modify(|n| *n += 1);
            let second = core.begin_exit(&t, gen, Some(1), "second".into()).unwrap();
            (first, second)
        };
        assert_eq!(close_exited(&inner, &t, &mut first).await, Closed::Recorded);
        assert_eq!(
            close_exited(&inner, &t, &mut second).await,
            Closed::Recorded
        );
        let evs = hub.thread_events(&t, 0, 10_000).unwrap().1;
        let n = |k: &str| evs.iter().filter(|e| e.kind() == k).count();
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind()).collect();
        assert_eq!(n("thread.crashed"), 1, "{kinds:?}");
        assert_eq!(n("turn.ended"), 1, "{kinds:?}");
        assert_eq!(
            hub.attention_list(true)
                .iter()
                .filter(|i| i["thread"] == t.as_str())
                .count(),
            1,
            "{kinds:?}"
        );
        hub.shutdown().await;
    }

    /// Review (item B): a kill of what was left of a runtime ([`reap_runtime`]) can still be
    /// in progress when a deletion's park has recorded the runtime's exit. The deletion waits
    /// for it, and the kill's end, should it come after the thread is gone, recreates nothing
    /// and never takes the count below zero (it panicked, or wrapped around in a release build).
    #[tokio::test]
    async fn a_deletion_waits_for_a_runtime_kill_whose_end_tolerates_the_thread_gone() {
        use twr_core::adapter::Answer;

        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(fake_hub_options(dir.path())).await.unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let created = hub
            .new_thread(
                NewThread {
                    project: proj.to_string_lossy().into_owned(),
                    runtime: Some("fake".into()),
                    name: Some("t".into()),
                    ..Default::default()
                },
                "c",
            )
            .await
            .unwrap();
        let t = created["id"].as_str().unwrap().to_string();
        let user = Caller::user("c");
        hub.park_thread(&t, true, true, &user).await.unwrap();
        let inner = Arc::clone(&hub.inner);
        // An end with no kill counted keeps the count at zero.
        {
            let mut core = inner.core();
            assert!(matches!(core.reaped(&t), AfterReap::Nothing));
            assert_eq!(core.rt[&t].reaping, 0);
        }
        // A kill still running after the park (as one started while the park waited).
        inner.core().rt(&t).reaping = 1;
        let card = hub
            .delete_threads(std::slice::from_ref(&t), &user)
            .await
            .unwrap();
        hub.respond_at(
            card["attn_id"].as_str().unwrap(),
            Answer::new("Delete"),
            card["revision"].as_u64(),
            &user,
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        {
            let core = inner.core();
            assert!(!core.registry.is_deleted(&t), "deleted during the kill");
            assert!(core.rt.get(&t).is_some_and(|r| r.deleting));
        }
        // The kill ends: the deletion goes on.
        assert!(matches!(inner.core().reaped(&t), AfterReap::Nothing));
        wait_core(&inner, |core| core.registry.is_deleted(&t)).await;
        {
            // A late end of another kill: nothing comes back for the deleted thread.
            let mut core = inner.core();
            assert!(matches!(core.reaped(&t), AfterReap::Nothing));
            assert!(!core.rt.contains_key(&t));
            // Nor for one that never was.
            assert!(matches!(core.reaped("nope"), AfterReap::Nothing));
            assert!(!core.rt.contains_key("nope"));
        }
        hub.shutdown().await;
    }

    #[test]
    fn runtime_attention_ids_carry_the_generation() {
        let id = runtime_attn_id("t1", 3, "perm-1");
        assert_eq!(id, "t1:3:perm-1");
        assert_eq!(split_runtime_attn_id("t1", &id), Some((Some(3), "perm-1")));
        // Ids the runtime made with colons of its own, and ids from before generations.
        assert_eq!(
            split_runtime_attn_id("t1", "t1:2:a:b"),
            Some((Some(2), "a:b"))
        );
        assert_eq!(
            split_runtime_attn_id("t1", "t1:perm-1"),
            Some((None, "perm-1"))
        );
        assert_eq!(split_runtime_attn_id("t1", "t2:1:x"), None);
        assert_eq!(split_runtime_attn_id("t1", "01HUBITEM"), None);
    }
}
