//! Closing sequences: the events that close a runtime's life (the open turn
//! interrupted, its adapter attention resolved, then `thread.stopped` / `crashed` / `attached` /
//! `parked`), planned from the log at each attempt to record them, and the bookkeeping of an
//! exit from [`Core::begin_exit`] until [`Core::finish_exit`].

use std::collections::{HashSet, VecDeque};

use chrono::{DateTime, Utc};
use serde_json::json;
use twr_core::event::*;

use super::runners::Fence;
use super::{
    twr_ext_map, AdapterScope, Batch, Core, Exit, Inner, Live, PendingExit, Planned,
    RuntimeExiting, TakenExit, HUB_SHUTDOWN,
};
use crate::store;

/// Longest `stderr_tail` kept on `thread.crashed`, in bytes.
const STDERR_TAIL_MAX: usize = 4096;

impl Core {
    /// The closing sequence for a runtime process that is gone. Refused
    /// ([`RuntimeExiting`]) while an exit of the thread's runtime is being closed: that exit
    /// records the closing sequence.
    pub(super) fn close_process(
        &mut self,
        hub: &Inner,
        thread: &str,
        exit: Exit,
    ) -> anyhow::Result<()> {
        if self.rt.get(thread).is_some_and(|r| r.exiting) {
            return Err(RuntimeExiting(thread.to_string()).into());
        }
        let mut plan = self.closing_plan(thread, exit)?;
        self.run_plan(hub, thread, &mut plan, None)
    }

    /// The events of the closing sequence for `thread`, computed up front and recorded as one
    /// batch ([`Core::run_plan`]): the open turn's end, the adapter items it closes and the
    /// `thread.stopped` / `thread.crashed` (+ its `error` item) / `thread.attached` /
    /// `thread.parked` are never half-recorded.
    pub(super) fn closing_plan(
        &self,
        thread: &str,
        exit: Exit,
    ) -> anyhow::Result<VecDeque<Planned>> {
        let view = self.view(thread).map_err(anyhow::Error::new)?;
        let mut plan = VecDeque::new();
        if let Some(turn) = view.current_turn {
            plan.push_back(Planned {
                turn: Some(turn),
                source: Source::Hub,
                body: EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Interrupted,
                    session_id: view.session_id.clone(),
                    ..Default::default()
                }),
            });
        }
        // The runtime is gone: every item it (or an earlier, closed runtime) raised is dead,
        // whatever turn it was raised in.
        for attn_id in self.open_adapter_attention(thread, AdapterScope::All, false) {
            plan.push_back(Planned {
                turn: view.current_turn,
                source: Source::Hub,
                body: EventBody::AttentionResolved(AttentionResolved {
                    attn_id,
                    by: ResolvedBy::ThreadStopped,
                    answer: None,
                    ..Default::default()
                }),
            });
        }
        match exit {
            Exit::Attach { client } => plan.push_back(Planned {
                turn: None,
                source: Source::User(client.clone()),
                body: EventBody::ThreadAttached(ClientPayload {
                    client,
                    ..Default::default()
                }),
            }),
            Exit::Park { source, keep } => plan.push_back(Planned {
                turn: None,
                source,
                body: EventBody::ThreadParked(SessionPayload {
                    session_id: view.session_id.clone(),
                    ext: twr_ext_map(json!({ "keep": keep })),
                    ..Default::default()
                }),
            }),
            // The log needs the stop (the process is gone); one the hub's shutdown asked for,
            // not the user, says so (`ext.twr.reason`), so viewers can fold those away, and so
            // does one the hub asked for to restart the runtime in new settings (`hub::restart`:
            // `mode_change`, `model_change` or `runtime_change`; the thread does not count as
            // stopped). A restart begun for a mode or model whose thread moved to another
            // runtime meanwhile is that move's: the runtime is replaced, not restarted.
            Exit::Stopped => {
                let rt = self.rt.get(thread);
                let shutdown = self.shutting_down && !rt.is_some_and(|r| r.stop_requested);
                let moved_away = rt
                    .and_then(|r| r.runtime_settings.as_ref())
                    .is_some_and(|s| !s.is_runtime_of(view));
                let reason = rt
                    .and_then(|r| r.restart_reason)
                    .map(|why| {
                        if moved_away {
                            RUNTIME_RESTART_REASON
                        } else {
                            why
                        }
                    })
                    .or(shutdown.then_some(HUB_SHUTDOWN));
                plan.push_back(Planned {
                    turn: None,
                    source: Source::Hub,
                    body: EventBody::ThreadStopped(SessionPayload {
                        session_id: view.session_id.clone(),
                        ext: reason.and_then(|r| twr_ext_map(json!({ "reason": r }))),
                        ..Default::default()
                    }),
                })
            }
            Exit::Crashed {
                exit_code,
                stderr_tail,
            } => {
                let tail = tail_utf8(&stderr_tail, STDERR_TAIL_MAX).to_string();
                let code = exit_code.map_or("signal".to_string(), |c| format!("exit {c}"));
                let last_line = tail.lines().rev().find(|l| !l.trim().is_empty());
                let summary = match last_line {
                    Some(l) => format!("{} crashed ({code}): {}", view.name, l.trim()),
                    None => format!("{} crashed ({code})", view.name),
                };
                plan.push_back(Planned {
                    turn: None,
                    source: Source::Hub,
                    body: EventBody::ThreadCrashed(ThreadCrashed {
                        exit_code,
                        stderr_tail: tail,
                        ..Default::default()
                    }),
                });
                // A crash in the turn that carried a resume (one that is no retry itself): the
                // resume failed, and the turn's automatic retry starts a new session
                // (`hub::switch`), so no item; one that is not retried after all gets its item
                // from the failure's reactions (`hub::retry`).
                let resume_failed = view.handoff_pending
                    && view.current_turn_retry_of.is_none()
                    && view
                        .resume_turn
                        .is_some_and(|t| view.current_turn == Some(t));
                // The item belongs to the turn the crash took down, if one was open (an
                // automatic retry of that turn resolves it, `hub::retry`).
                if !resume_failed {
                    plan.push_back(Planned {
                        turn: view.current_turn,
                        source: Source::Hub,
                        body: EventBody::AttentionRaised(AttentionRaised {
                            attn_id: new_id(),
                            reason: AttentionReason::Error,
                            summary,
                            options: vec!["dismiss".into()],
                            ..Default::default()
                        }),
                    });
                }
            }
        }
        Ok(plan)
    }

    /// Records the planned events as one batch and empties the plan. On failure nothing is
    /// recorded and the plan is kept, so the caller can retry it as a whole ([`persist`]);
    /// a hub that dies instead finds the thread still open at startup and closes it then.
    /// `ack`: the runner frame (`exited`) the plan comes from, committed with it.
    ///
    /// [`persist`]: super::supervisor::persist
    fn run_plan(
        &mut self,
        hub: &Inner,
        thread: &str,
        plan: &mut VecDeque<Planned>,
        ack: Option<&store::RunnerAck>,
    ) -> anyhow::Result<()> {
        self.run_plan_at(hub, thread, plan, ack, None)
    }

    /// [`Core::run_plan`] for an exit that happened `at` (a runner's `exited` frame, see
    /// [`Batch::at`]).
    pub(super) fn run_plan_at(
        &mut self,
        hub: &Inner,
        thread: &str,
        plan: &mut VecDeque<Planned>,
        ack: Option<&store::RunnerAck>,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        if plan.is_empty() {
            return self.commit_ack(hub, ack);
        }
        let mut batch = Batch {
            runner_ack: ack.cloned(),
            at,
            ..Batch::default()
        };
        for p in plan.iter() {
            batch.push(thread, p.turn, p.source.clone(), p.body.clone());
        }
        self.append_batch(hub, batch)?;
        plan.clear();
        Ok(())
    }

    /// Open attention items of `thread` that an adapter raised within `scope` (only those
    /// blocking the thread, permission / question, when `blocking_only`).
    pub(super) fn open_adapter_attention(
        &self,
        thread: &str,
        scope: AdapterScope,
        blocking_only: bool,
    ) -> Vec<String> {
        self.attention
            .for_thread(thread, &HashSet::new())
            .into_iter()
            .filter(|i| !blocking_only || i.reason.is_blocking())
            .filter(|i| {
                self.adapter_attn
                    .get(&i.attn_id)
                    .is_some_and(|raised_in| scope.covers(thread, &i.attn_id, *raised_in))
            })
            .map(|i| i.attn_id.clone())
            .collect()
    }

    /// Resolves (`by`, no answer, source hub) every open attention item of `thread` that
    /// runtime generation `gen` raised in turn `turn` or before ([`AdapterScope::Turn`]; only
    /// those blocking the thread, permission / question, when `blocking_only`): what the end
    /// of `turn` closes. A later turn's items (a retry of this cleanup that runs after the next
    /// turn started) and the items raised between turns stay open. Items already resolved are
    /// skipped, so this is safe to repeat; each resolution stands on its own, so a failure
    /// part-way leaves the rest for the retry.
    pub(super) fn resolve_turn_adapter_attention(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        gen: u64,
        by: ResolvedBy,
        blocking_only: bool,
    ) -> anyhow::Result<()> {
        let scope = AdapterScope::Turn { turn, gen };
        for attn_id in self.open_adapter_attention(thread, scope, blocking_only) {
            self.append(
                hub,
                thread,
                Some(turn),
                Source::Hub,
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id,
                    by,
                    answer: None,
                    ..Default::default()
                }),
            )?;
        }
        Ok(())
    }

    /// Whether an adapter's `attention.resolved` is a withdrawal the hub accepts: `by: timeout`
    /// with no answer, for an item still open on `thread` that an adapter (this thread's
    /// runtime: every other runtime's items were closed when it exited) raised.
    pub(super) fn is_adapter_withdrawal(&self, thread: &str, p: &AttentionResolved) -> bool {
        p.by == ResolvedBy::Timeout
            && p.answer.is_none()
            && self.adapter_attn.contains_key(&p.attn_id)
            && self
                .attention
                .get(&p.attn_id)
                .is_some_and(|i| i.thread == thread)
    }

    /// Takes the thread's live runtime if it is `generation`. The caller runs the closing
    /// sequence and then calls [`release_live`], so that "no live runtimes" implies every
    /// closing event is already appended.
    pub(super) fn remove_live(&mut self, thread: &str, generation: u64) -> Option<Live> {
        let rt = self.rt.get_mut(thread)?;
        if rt.live.as_ref().map(|l| l.generation) != Some(generation) {
            return None;
        }
        rt.live.take()
    }

    /// Takes the exited runtime `generation` of `thread` and decides how it ended (`None` when
    /// the hub already closed it). The caller records its closing sequence ([`close_exited`],
    /// planned at each attempt by [`Core::exit_plan`]), then [`Core::finish_exit`].
    ///
    /// [`close_exited`]: super::supervisor::close_exited
    pub(super) fn begin_exit(
        &mut self,
        thread: &str,
        generation: u64,
        exit_code: Option<i32>,
        stderr_tail: String,
    ) -> Option<PendingExit> {
        self.begin_exit_keeping(thread, generation, exit_code, stderr_tail, None)
    }

    /// [`Core::begin_exit`] of an exit a runner connection brought (its `exited` frame, or a
    /// runtime the reconciliation at its `synced` found lost): the exit carries the
    /// connection's `fence`, and the thread keeps the runtime it took, to be put back should
    /// the box's token change before the closing sequence commits ([`ThreadRt::taken`]).
    ///
    /// [`ThreadRt::taken`]: super::ThreadRt::taken
    pub(super) fn begin_exit_on(
        &mut self,
        thread: &str,
        generation: u64,
        exit_code: Option<i32>,
        stderr_tail: String,
        fence: &Fence,
    ) -> Option<PendingExit> {
        self.begin_exit_keeping(thread, generation, exit_code, stderr_tail, Some(fence))
    }

    /// Puts back the runtimes of the exits whose connection proved a token its box no longer
    /// has ([`Fence::revoked`]), as they were before their exit began: live, not exiting.
    /// Nothing else changed since (those exits recorded nothing, and record nothing now:
    /// `supervisor::close_exited`). Called by `box.token` in the step under the core lock
    /// that changes the token (`runners::set_box_token`), so the box's next connection
    /// finds them back.
    pub(super) fn put_back_revoked_exits(&mut self) {
        for (thread, rt) in &mut self.rt {
            if !rt.taken.as_ref().is_some_and(|t| t.fence.revoked()) {
                continue;
            }
            if let Some(taken) = rt.taken.take() {
                tracing::info!(
                    thread,
                    "an exit a connection brought whose token the box no longer has: the \
                     runtime put back (the box's next connection reports it again)"
                );
                rt.live = Some(*taken.live);
                rt.exiting = false;
            }
        }
    }

    fn begin_exit_keeping(
        &mut self,
        thread: &str,
        generation: u64,
        exit_code: Option<i32>,
        stderr_tail: String,
        fence: Option<&Fence>,
    ) -> Option<PendingExit> {
        let live = self.remove_live(thread, generation)?;
        let rt = self.rt(thread);
        rt.exiting = true;
        // A runtime the hub gave up on ([`give_up_runtime`]) crashed, whatever else asked it to
        // stop meanwhile (a hub shutdown), unless the user did. (Left in the runtime, which an
        // exit that is put back keeps as it was.)
        let given_up = live.given_up.clone();
        // A durable `thread.stop` counts even when this runtime was taken over after a hub
        // restart (the in-memory flag is then gone).
        let stopped = rt.stop_requested || (live.stop_requested && given_up.is_none());
        let attach_client = self.attach_client(thread, &live);
        let park = if attach_client.is_none() {
            self.park_exit(thread, &live)
        } else {
            None
        };
        let parking = park.is_some();
        let crash = attach_client.is_none() && !parking && !stopped;
        let exit = match (&attach_client, park) {
            (Some(client), _) => Exit::Attach {
                client: client.clone(),
            },
            (None, Some(park)) => park,
            (None, None) if stopped => Exit::Stopped,
            (None, None) => Exit::Crashed {
                exit_code,
                stderr_tail: given_up.clone().unwrap_or(stderr_tail),
            },
        };
        let restart = stopped && given_up.is_none() && rt_restart(self, thread);
        let exit = PendingExit {
            exit,
            log_runtime: live.log_runtime,
            attach_client,
            restart,
            // A failure, not a stop: the queue is not dispatched after it.
            stopped: stopped && given_up.is_none(),
            park: parking,
            crash,
            runner_ack: None,
            fence: fence.cloned(),
            at: None,
        };
        if let Some(fence) = fence {
            self.rt(thread).taken = Some(TakenExit {
                live: Box::new(live),
                fence: fence.clone(),
            });
        }
        Some(exit)
    }

    /// The closing sequence of `exit`, planned from the state now (at each attempt to record
    /// it: whatever went in meanwhile, e.g. an answer resolving an item, is never recorded
    /// twice). A `thread.stop` that came after the exit was taken as a crash (while it was
    /// being closed) turns it into a stop. `None` when the log has closed the exited runtime
    /// already (another closing sequence went in first): nothing is to be recorded but the
    /// exit's frame.
    pub(super) fn exit_plan(
        &self,
        thread: &str,
        exit: &mut PendingExit,
    ) -> Option<VecDeque<Planned>> {
        if self.runtime_gen(thread) != exit.log_runtime {
            return None;
        }
        if exit.crash && self.rt.get(thread).is_some_and(|r| r.stop_requested) {
            exit.exit = Exit::Stopped;
            exit.crash = false;
            exit.stopped = true;
        }
        // A `thread.stop` that came meanwhile wins over a mode restart.
        exit.restart = exit.restart && rt_restart(self, thread);
        Some(match self.closing_plan(thread, exit.exit.clone()) {
            Ok(plan) => plan,
            Err(e) => {
                // Only an unknown thread gets here, and it has nothing to close.
                tracing::error!(thread, error = %e, "cannot plan the closing sequence");
                VecDeque::new()
            }
        })
    }

    /// After an exit's closing sequence ran (`closed`): releases the runtime slot and wakes
    /// `attach.begin`. Returns whether the queue should be dispatched now. Called under the
    /// core lock the closing sequence was committed under ([`close_exited`]), so nothing sees
    /// the log's runtime closed while the thread still counts as `exiting`.
    ///
    /// [`close_exited`]: super::supervisor::close_exited
    pub(super) fn finish_exit(
        &mut self,
        hub: &Inner,
        thread: &str,
        exit: &PendingExit,
        closed: bool,
    ) -> bool {
        release_live(hub);
        if let Some(rt) = self.rt.get_mut(thread) {
            rt.exiting = false;
            rt.taken = None;
            if closed {
                rt.restart_reason = None;
            }
        }
        if exit.attach_client.is_some() {
            self.attach_granted(thread, closed);
        }
        if exit.park {
            self.park_quiesced(thread, closed);
        }
        // Prompts sent while the runtime was stopping start a fresh (resumed) session.
        closed && exit.stopped && exit.attach_client.is_none() && !exit.park
    }
}

/// Whether `thread`'s runtime is being stopped for a settings restart (`hub::restart`).
fn rt_restart(core: &Core, thread: &str) -> bool {
    core.rt
        .get(thread)
        .is_some_and(|r| r.restart_reason.is_some())
}

pub(super) fn release_live(hub: &Inner) {
    hub.live_count.send_modify(|n| *n = n.saturating_sub(1));
}

/// Last at most `max` bytes of `s`, on a character boundary.
fn tail_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails_end_on_a_character_boundary() {
        assert_eq!(tail_utf8("abcdef", 3), "def");
        assert_eq!(tail_utf8("éé", 3), "é");
    }
}
