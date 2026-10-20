//! `say`: routing a user message with the decision ladder, and the hub questions it raises
//! when nobody can decide.
//!
//! The route ladder (code rules, then the orchestrator when one is configured) runs over a
//! snapshot of the registry and the attention stack, taken under the lock; what it routes is
//! recorded under the lock again afterwards. A thread it picked that stopped or disappeared
//! in between is not sent the message: the user is asked
//! instead (the decisions are logged either way). Every rung that answers is
//! logged as `decision.made{site: route}` on the reserved `hub` thread, and the
//! accepted one is the `routed_by` of the turn it starts. Outcomes:
//!
//! - send to a thread: `thread.send` from the user;
//! - answer a thread's open permission/question: `attention.respond` as the user, only for an
//!   item the user had seen: raised at or before `seen_gseq`, the control gseq the client had
//!   rendered when the user wrote the message. Without `seen_gseq` (`twr say`) a message never
//!   answers an item (use `twr respond`): one that looks like an answer to a waiting
//!   permission/question (a strict answer or [`answer_shaped`], to the addressed thread's item
//!   or any thread's) is not routed at all; `say` returns `action: "pending"` listing the open
//!   items instead, and records nothing. With `seen_gseq`, a strict answer to an unseen item
//!   makes the code rule abstain, so it is not sent as text either.
//!
//!   [`answer_shaped`]: twr_decisions::code::route::answer_shaped "Raised" is the item's latest
//!   `attention.raised`: an open item raised again after `seen_gseq` (a new revision, which may
//!   ask something else) is unseen, and this is checked again under the lock that records the
//!   answer (`conflict` when it was raised again while the ladder ran);
//! - `new <project>: <message>`: a new thread in that project (default runtime), then the
//!   message;
//! - delegate: the message, verbatim, as a user turn of the orchestrator (created on first use);
//! - ask the user: `attention.raised{reason: question}` on the `hub` thread, options = worker
//!   names + [`NEW_THREAD`] (`new thread`) + [`CANCEL_OPTION`] (`cancel (don't send)`), with
//!   the message kept in `ext.twr.pending_message` (so it is in the log). Answering it routes
//!   the message to the chosen thread (`decision.made{provider: user}`), or to the
//!   orchestrator for `new thread` (an error when none is configured); `cancel (don't send)`
//!   (or `attention.dismiss`) resolves it and routes nothing. The two actions have spaces in
//!   their labels, so they never collide with a thread name (a worker named `cancel` or
//!   `new_thread` is routed to like any other).
//!
//! `say {to: "orchestrator"}` skips the ladder: the user picked the orchestrator (the TUI's
//! orchestrator conversation), and the message is delegated as above
//! ([`Hub::say_to_orchestrator`]).
//!
//! A message `say` queues for the orchestrator carries `said_after` = `min(seen_gseq, head)`
//! (0 without `seen_gseq`; kept in the hub question's `ext.twr.said_after` until it is routed),
//! so the relay guard (`hub::orch`) lets it answer only items the user had seen. Its routing
//! decision is tagged `ext.twr.user_prompt_for` (`Core::push_prompt`): the message's arrival,
//! which ends the relay and mode authority of the orchestrator's turn in progress.

use std::collections::HashSet;

use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::AttentionItem;
use twr_decisions::code::route::unseen_answer;
use twr_decisions::{
    DecisionContext, ReplyAnswer, RouteContext, RouteOutcome, DELEGATE, NEW_THREAD,
};

use super::{
    append_err, rpc, twr_ext, twr_ext_map, AnswerFor, Batch, Caller, Core, Hub, Inner, NewThread,
    Response, RoutedBy,
};

/// Longest message `say` accepts, in bytes.
pub const MAX_MESSAGE: usize = 256 * 1024;

/// The answer `attention.dismiss` records.
pub const CANCEL: &str = "cancel";

/// The option of a hub question that routes its message nowhere. It has a space, which no
/// thread name has, so it can't be a thread's option: a worker named `cancel` stays routable.
/// The same holds for [`NEW_THREAD`].
pub const CANCEL_OPTION: &str = "cancel (don't send)";

/// What the cancel and new-thread options of hub questions were called before they got labels
/// no thread name can take. A question raised then (its options have neither current label)
/// keeps reading them that way.
const LEGACY_CANCEL: &str = "cancel";
const LEGACY_NEW_THREAD: &str = "new_thread";

/// Whether hub question `item` predates [`CANCEL_OPTION`] (see [`LEGACY_CANCEL`]).
fn legacy_question(item: &AttentionItem) -> bool {
    !item
        .options
        .iter()
        .any(|o| o == CANCEL_OPTION || o == NEW_THREAD)
}

/// Whether `decision`, an answer to hub question `item`, drops its message.
pub(super) fn cancels(item: &AttentionItem, decision: &str) -> bool {
    decision == CANCEL_OPTION || (decision == LEGACY_CANCEL && legacy_question(item))
}

/// Whether `decision`, an answer to hub question `item`, hands its message to the
/// orchestrator.
fn picks_new_thread(item: &AttentionItem, decision: &str) -> bool {
    decision == NEW_THREAD || (decision == LEGACY_NEW_THREAD && legacy_question(item))
}

/// How much of the message a hub question quotes.
const PREVIEW_CHARS: usize = 80;

/// The message a hub question routes once answered.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PendingRoute {
    /// The user's message.
    pub(super) message: String,
    /// The client that said it.
    pub(super) asked_by: Option<String>,
    /// What the user had seen when saying it (the `said_after` of its queued prompt; see the
    /// module docs). `None` in a question logged before it was recorded: treated as 0.
    pub(super) said_after: Option<u64>,
}

impl PendingRoute {
    /// From a hub question's `ext.twr.pending_message` / `ext.twr.asked_by`.
    pub(super) fn from_raised(p: &AttentionRaised) -> Option<PendingRoute> {
        let twr = twr_ext(&p.ext)?;
        Some(PendingRoute {
            message: twr.get("pending_message")?.as_str()?.to_string(),
            asked_by: twr
                .get("asked_by")
                .and_then(Value::as_str)
                .map(str::to_string),
            said_after: twr.get("said_after").and_then(Value::as_u64),
        })
    }
}

/// Where an answered hub question sends its message.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum RouteTarget {
    /// A thread (id).
    Thread(String),
    /// The orchestrator (created if needed).
    Orchestrator,
}

/// One line, at most [`PREVIEW_CHARS`] characters.
fn preview(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(PREVIEW_CHARS - 3).collect();
    format!("{}...", cut.trim_end())
}

impl Core {
    /// Checks an answer to hub question `item` before it is recorded: a thread (id, name,
    /// alias) or [`NEW_THREAD`] (the orchestrator; the orchestrator itself may not pick it).
    pub(super) fn hub_answer_target(
        &self,
        hub: &Inner,
        item: &AttentionItem,
        decision: &str,
        caller: &Caller,
    ) -> Result<RouteTarget, RpcError> {
        if picks_new_thread(item, decision) {
            if caller.is_orchestrator() {
                return Err(rpc(
                    codes::RELAY_FORBIDDEN,
                    "the orchestrator cannot hand the user's message to itself; the user decides",
                ));
            }
            if !self.orchestrator_available(hub) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "no orchestrator is configured to start a new thread (runtime {:?} is \
                         not available); answer with a thread, or use `new <project>: ...`",
                        self.orchestrator_choice(hub).0
                    ),
                ));
            }
            return Ok(RouteTarget::Orchestrator);
        }
        let id = self.resolve(decision).map_err(|_| {
            rpc(
                codes::BAD_REQUEST,
                format!(
                    "{decision:?} is not a thread; answer with one of {:?}",
                    item.options
                ),
            )
        })?;
        if self.orch.thread.as_deref() == Some(id.as_str()) {
            return Ok(RouteTarget::Orchestrator);
        }
        Ok(RouteTarget::Thread(id))
    }

    /// The thread an answered hub question's message goes to: the chosen thread, or the
    /// orchestrator (created if needed). Checks it can take a prompt now (the hub is not
    /// shutting down), so recording the answer is the last thing that can fail before
    /// [`Core::route_hub_answer`].
    pub(super) fn hub_answer_thread(
        &mut self,
        hub: &Inner,
        target: RouteTarget,
    ) -> Result<String, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        match target {
            RouteTarget::Thread(id) => {
                self.view(&id)?;
                Ok(id)
            }
            RouteTarget::Orchestrator => self.ensure_orchestrator(hub),
        }
    }

    /// Adds to `batch` the answer to hub question `item` and the routing of its message to
    /// `thread` ([`Core::hub_answer_thread`]): `resolved` (the item's `attention.resolved`),
    /// the answer as `decision.made{site: route, provider: user | orchestrator}` on the hub
    /// thread, and the message queued for `thread` as the user who said it, with that decision
    /// as `routed_by`. Recorded as one batch, a crash or write failure leaves either the
    /// question open (answer it again) or all three recorded; the queue is dispatched after
    /// the commit (and at startup). Returns the index of the queued prompt among the batch's
    /// prompts. On an error `batch` is left as it was.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stage_hub_answer(
        &self,
        batch: &mut Batch,
        item: &AttentionItem,
        resolved: EventBody,
        decision: &str,
        pending: &PendingRoute,
        thread: &str,
        caller: &Caller,
    ) -> Result<usize, RpcError> {
        let provider = if caller.is_orchestrator() {
            DecisionProvider::Orchestrator
        } else {
            DecisionProvider::User
        };
        let (events, prompts) = (batch.events.len(), batch.prompts.len());
        let turn = self.registry.get(&item.thread).and_then(|v| v.current_turn);
        batch.push(&item.thread, turn, caller.source(), resolved);
        let d = batch.push(
            HUB_THREAD,
            None,
            caller.source(),
            EventBody::DecisionMade(DecisionMade {
                site: DecisionSite::Route,
                provider,
                options: item.options.clone(),
                chosen: decision.to_string(),
                confidence: 1.0,
                latency_ms: 0,
                ..Default::default()
            }),
        );
        let source = match &pending.asked_by {
            Some(c) => Source::User(c.clone()),
            None => caller.source(),
        };
        if let Err(e) = self.push_prompt(
            batch,
            thread,
            &pending.message,
            TurnFrom::User,
            source,
            Some(RoutedBy::Event(d)),
        ) {
            batch.events.truncate(events);
            batch.prompts.truncate(prompts);
            return Err(e);
        }
        // The user's answer sends the user's message; the orchestrator's does not release a
        // held queue (`Batch::user_send`).
        if !caller.is_orchestrator() {
            batch.user_send();
        }
        cap_said_after(batch, pending.said_after.unwrap_or(0));
        Ok(batch.prompts.len() - 1)
    }
}

/// A batch holding the route ladder's `records` on the hub thread, and the accepted one (the
/// last record, when the ladder `decided`) as a `routed_by`.
fn route_records(records: Vec<DecisionMade>, decided: bool) -> (Batch, Option<RoutedBy>) {
    let mut batch = Batch::default();
    let mut last = None;
    for d in records {
        let source = Source::Decision(d.provider.as_str().into());
        last = Some(batch.push(HUB_THREAD, None, source, EventBody::DecisionMade(d)));
    }
    (batch, last.filter(|_| decided).map(RoutedBy::Event))
}

/// Whether `thread`, chosen from the snapshot `ctx`, can still get the message: it exists and
/// was not stopped since the snapshot (a thread the user addressed while it was stopped stays
/// addressable).
fn still_routable(core: &Core, ctx: &RouteContext, thread: &str) -> bool {
    match (core.registry.get(thread), ctx.thread(thread)) {
        (Some(now), Some(then)) => !now.stopped || then.stopped,
        _ => false,
    }
}

/// Caps the `said_after` of the prompt `batch` just queued at `seen` (what the user had seen
/// when saying it): the relay guard then lets that message answer only items raised by then.
fn cap_said_after(batch: &mut Batch, seen: u64) {
    if let Some(p) = batch.prompts.last_mut() {
        p.said_after = Some(p.said_after.map_or(seen, |s| s.min(seen)));
    }
}

/// `say`'s `seen_gseq` as a `said_after` cap: at most the control head, 0 when absent.
fn seen_cap(seen_gseq: Option<u64>, head: u64) -> u64 {
    seen_gseq.map_or(0, |s| s.min(head))
}

/// `say`'s result for an answer said with nothing on screen ([`unseen_answer`]): nothing is
/// routed or recorded; `items` are the open permissions/questions of the worker threads (of
/// `thread` only, when the message addressed it), with their revisions, for `twr respond
/// <attn_id>@<revision> <answer>`.
fn pending_answers(core: &Core, thread: Option<&str>) -> Value {
    let items: Vec<Value> = core
        .attention
        .ordered(&HashSet::new())
        .into_iter()
        .filter(|i| i.reason.is_blocking() && i.thread != HUB_THREAD)
        .filter(|i| thread.is_none_or(|t| i.thread == t))
        .map(|i| {
            json!({
                "attn_id": i.attn_id,
                "revision": i.revision,
                "thread": i.thread,
                "thread_name": core.registry.get(&i.thread).map_or(i.thread.as_str(), |v| v.name.as_str()),
                "reason": i.reason,
                "summary": i.summary,
                "options": i.options,
            })
        })
        .collect();
    json!({ "action": "pending", "items": items, "decision_ids": [] })
}

/// Ids of the route decisions of a committed [`route_records`] batch (its first `n` events).
fn decision_ids(events: &[Event], n: usize) -> Vec<String> {
    events.iter().take(n).map(|e| e.id.clone()).collect()
}

impl Hub {
    /// `say {message, focused?}` without `seen_gseq`: [`Hub::say_seen`] with `None` (the
    /// message never answers an item).
    pub async fn say(
        &self,
        message: &str,
        focused: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        self.say_seen(message, focused, None, caller).await
    }

    /// `say {message, focused?, seen_gseq?}`: routes a user message (see the module docs).
    /// `focused` is the client's focused thread (default: the leftmost in the stack);
    /// `seen_gseq` the control gseq it had rendered (only items raised at or before it can be
    /// answered). Returns `{action, thread?, thread_name?, ref?, turn?, attn_id?, options?,
    /// decision_ids}` with `action` one of `send`, `respond`, `new_thread`, `delegate`, `ask`
    /// (`ref`: the queued message's, when one was queued: `send`, `delegate`, `new_thread`);
    /// or, without `seen_gseq`, for a message that tries to answer a waiting permission or
    /// question, `{action: "pending", items: [{attn_id, revision, thread, thread_name, reason,
    /// summary, options}]}` with nothing routed or recorded (see [`pending_answers`]).
    pub async fn say_seen(
        &self,
        message: &str,
        focused: Option<&str>,
        seen_gseq: Option<u64>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "say routes the user's messages; the orchestrator uses thread.send",
            ));
        }
        let message = message.trim();
        if message.is_empty() {
            return Err(rpc(codes::BAD_REQUEST, "message is empty"));
        }
        if message.len() > MAX_MESSAGE {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("message is longer than {MAX_MESSAGE} bytes"),
            ));
        }
        let ctx = {
            let core = self.inner.core();
            if core.shutting_down {
                return Err(rpc(codes::CONFLICT, "hub is shutting down"));
            }
            let focused = match focused.filter(|f| !f.is_empty() && *f != HUB_THREAD) {
                Some(f) => Some(core.resolve(f)?),
                None => None,
            };
            let mut ctx = RouteContext::from_registry(
                &core.registry,
                &core.attention,
                &HashSet::new(),
                focused.as_deref(),
                message,
                core.orchestrator_available(&self.inner),
            );
            // Only what the user had seen can be answered.
            ctx.set_answerable(|attn| {
                let raised = core.orch.raised_gseq(attn);
                matches!((raised, seen_gseq), (Some(r), Some(seen)) if r <= seen)
            });
            // Nothing on screen (`twr say`): an answer to a waiting permission/question is
            // never given, and routing it would only ask "which thread?" or send
            // "yes" as a prompt. Say so instead, naming the items to answer with `twr respond`.
            if seen_gseq.is_none() {
                if let Some(target) = unseen_answer(&ctx, message) {
                    return Ok(pending_answers(&core, target.as_deref()));
                }
            }
            ctx
        };
        let options = ctx.options();
        let run = self
            .inner
            .ladder
            .run(&DecisionContext::Route(ctx.clone()), &options);
        let outcome = run.route(&ctx);
        let decided = run.decision().is_some();
        let records: Vec<DecisionMade> = run.records.into_iter().map(DecisionMade::from).collect();
        let n = records.len();
        let mut out = json!({});
        let name_of = |core: &Core, id: &str| core.registry.get(id).map(|v| v.name.clone());
        // Sending, delegating and asking record the ladder's decisions in one batch with what
        // they route (the queued message, or the question holding it): a crash or write
        // failure leaves all of it or none of it (nothing half-routed, and the caller, who got
        // an error, still has the message). Answering an item and creating a thread log the
        // decisions first (see below).
        match outcome {
            RouteOutcome::SendTo {
                thread,
                message: text,
            } => {
                let mut core = self.inner.core();
                if !still_routable(&core, &ctx, &thread) {
                    // The ladder ran over a snapshot and the target stopped or went away
                    // before the lock was taken again: the decisions are logged all the same,
                    // and the user is asked, with the message kept in the question.
                    tracing::info!(%thread, "the routed thread changed while deciding; asking the user");
                    let options = RouteContext::from_registry(
                        &core.registry,
                        &core.attention,
                        &HashSet::new(),
                        None,
                        message,
                        core.orchestrator_available(&self.inner),
                    )
                    .options();
                    drop(core);
                    self.ask_user(
                        records, false, message, options, seen_gseq, caller, &mut out,
                    )?;
                    return Ok(out);
                }
                let (mut batch, routed_by) = route_records(records, decided);
                core.push_prompt(
                    &mut batch,
                    &thread,
                    &text,
                    TurnFrom::User,
                    caller.source(),
                    routed_by,
                )?;
                batch.user_send();
                cap_said_after(&mut batch, seen_cap(seen_gseq, core.gseq));
                let done = core.append_batch(&self.inner, batch).map_err(append_err)?;
                out["decision_ids"] = json!(decision_ids(&done.events, n));
                let queued = done.queued.first().copied().unwrap_or_default();
                let sent = core.dispatch_queued(&self.inner, &thread, queued);
                out["action"] = json!("send");
                out["thread_name"] = json!(name_of(&core, &thread));
                out["thread"] = json!(thread);
                out["ref"] = json!(sent.msg_ref);
                out["turn"] = json!(sent.turn);
                out["pending"] = json!(sent.pending);
            }
            // Partial application is safe: the decisions are an audit trail that routes
            // nothing by itself; if the answer then fails, the item stays open and the caller
            // gets the error.
            RouteOutcome::RespondAttention {
                thread,
                attn_id,
                answer,
            } => {
                out["decision_ids"] = json!(self.log_route_decisions(records)?);
                // Only the revision the user had seen (the ladder ran with no lock held, and
                // the item may have been raised again meanwhile).
                let response = match answer {
                    ReplyAnswer::Label(l) => Response::Answer(Answer::new(l)),
                    ReplyAnswer::OptionIndex(index) => Response::Option {
                        index,
                        message: None,
                    },
                };
                let ev = self.respond_seen(
                    &attn_id,
                    response,
                    caller,
                    AnswerFor::SeenBy(seen_gseq.unwrap_or(0)),
                )?;
                let answer = match &ev.body {
                    EventBody::AttentionResolved(p) => {
                        p.answer.as_ref().map(|a| a.decision.clone())
                    }
                    _ => None,
                };
                out["action"] = json!("respond");
                out["thread_name"] = json!(name_of(&self.inner.core(), &thread));
                out["thread"] = json!(thread);
                out["attn_id"] = json!(attn_id);
                out["answer"] = json!(answer);
            }
            // The thread is made (with its worktree) outside the core lock, so it cannot share
            // a batch with its message. A failure after `thread.created` leaves an idle thread
            // and returns the error; the caller, who still has the message, sends it to that
            // thread. Nothing that was acknowledged is lost.
            RouteOutcome::NewThreadRequest { project, message } => {
                let ids = self.log_route_decisions(records)?;
                let routed_by = if decided { ids.last().cloned() } else { None };
                out["decision_ids"] = json!(ids);
                let t = self
                    .new_thread_by(
                        NewThread {
                            project,
                            ..Default::default()
                        },
                        caller,
                    )
                    .await?;
                let id = t["id"].as_str().unwrap_or_default().to_string();
                out["action"] = json!("new_thread");
                out["thread_name"] = t["name"].clone();
                out["thread"] = json!(id);
                if !message.is_empty() {
                    let mut core = self.inner.core();
                    let sent = core.queue_prompt(
                        &self.inner,
                        &id,
                        &message,
                        TurnFrom::User,
                        caller.source(),
                        routed_by.as_deref(),
                    )?;
                    out["ref"] = json!(sent.msg_ref);
                    out["turn"] = json!(sent.turn);
                }
            }
            RouteOutcome::Delegate => {
                let mut core = self.inner.core();
                // Creating the orchestrator is its own append: it is idempotent (an existing
                // orchestrator is reused), so a failure after it loses nothing.
                let orch = core.ensure_orchestrator(&self.inner)?;
                let (mut batch, routed_by) = route_records(records, decided);
                core.push_prompt(
                    &mut batch,
                    &orch,
                    message,
                    TurnFrom::User,
                    caller.source(),
                    routed_by,
                )?;
                batch.user_send();
                cap_said_after(&mut batch, seen_cap(seen_gseq, core.gseq));
                let done = core.append_batch(&self.inner, batch).map_err(append_err)?;
                out["decision_ids"] = json!(decision_ids(&done.events, n));
                let queued = done.queued.first().copied().unwrap_or_default();
                let sent = core.dispatch_queued(&self.inner, &orch, queued);
                out["action"] = json!("delegate");
                out["thread_name"] = json!(name_of(&core, &orch));
                out["thread"] = json!(orch);
                out["ref"] = json!(sent.msg_ref);
                out["turn"] = json!(sent.turn);
                out["pending"] = json!(sent.pending);
            }
            RouteOutcome::AskUser { options } => {
                self.ask_user(
                    records, decided, message, options, seen_gseq, caller, &mut out,
                )?;
            }
        }
        Ok(out)
    }

    /// `say {message, to: "orchestrator", seen_gseq?}`: the user picked the orchestrator
    /// themselves (the TUI's orchestrator conversation), so the route ladder does not run:
    /// the message is handed to the orchestrator exactly as the ladder's `delegate` outcome
    /// does (a user turn, queued with `said_after` capped at `seen_gseq`, so the relay guard
    /// judges it like any delegated message), with the user's choice logged as
    /// `decision.made{site: route, provider: user, chosen: "orchestrator"}` (its `routed_by`).
    /// Returns `{action: "delegate", thread, thread_name, ref, turn, pending, decision_ids}`
    /// (`ref`: the message's, for `message.withdraw`), or, for a short command the hub applied
    /// at once, `{action: "command", applied, ...}` with no ref: nothing is queued to take
    /// back.
    /// Without `seen_gseq`, an answer attempt is refused as `say` refuses it
    /// (`action: "pending"`, nothing recorded). No orchestrator available (its runtime is
    /// missing): `conflict`, nothing recorded (the ladder would ask "which thread?", which
    /// is not what the user asked for).
    pub fn say_to_orchestrator(
        &self,
        message: &str,
        seen_gseq: Option<u64>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        self.say_to_orchestrator_as(message, seen_gseq, caller, false)
    }

    /// [`Hub::say_to_orchestrator`]; `suggested`: the message is a suggestion the user took
    /// unedited (`hub::suggest`), never applied as a short command at once: it goes to the
    /// orchestrator as a message, whatever it reads like now.
    pub fn say_to_orchestrator_as(
        &self,
        message: &str,
        seen_gseq: Option<u64>,
        caller: &Caller,
        suggested: bool,
    ) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "say routes the user's messages; the orchestrator uses thread.send",
            ));
        }
        let message = message.trim();
        if message.is_empty() {
            return Err(rpc(codes::BAD_REQUEST, "message is empty"));
        }
        if message.len() > MAX_MESSAGE {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("message is longer than {MAX_MESSAGE} bytes"),
            ));
        }
        let mut core = self.inner.core();
        if core.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let available = core.orchestrator_available(&self.inner);
        // Nothing on screen: an answer is never given, as for any `say`.
        if seen_gseq.is_none() {
            let ctx = RouteContext::from_registry(
                &core.registry,
                &core.attention,
                &HashSet::new(),
                None,
                message,
                available,
            );
            if let Some(target) = unseen_answer(&ctx, message) {
                return Ok(pending_answers(&core, target.as_deref()));
            }
        }
        if !available {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "no orchestrator is available (runtime {:?} is not available): nothing was \
                     sent; say it without `to` to route it, or switch the orchestrator with \
                     `twr orchestrator use <runtime>`",
                    core.orchestrator_choice(&self.inner).0
                ),
            ));
        }
        // Creating the orchestrator is its own append: it is idempotent (an existing
        // orchestrator is reused), so a failure after it loses nothing.
        // A suggestion is never a command. One that reads as one now (a thread it names was
        // made since it was suggested, an item it answers raised) would carry a command's
        // authority into the orchestrator's turn (`Core::mode_check`, `Core::relay_check`
        // read the message against the threads and items as they are when it arrives).
        if suggested && core.reads_as_command(message) {
            return Err(rpc(
                codes::BAD_REQUEST,
                "this suggestion now reads as an answer or a mode change, and a suggestion never                  gives one: nothing was sent; type it yourself if you mean it",
            ));
        }
        let orch = core.ensure_orchestrator(&self.inner)?;
        let seen = seen_cap(seen_gseq, core.gseq);
        if !suggested {
            if let Some(result) = core.apply_command(&self.inner, &orch, message, seen, caller)? {
                return Ok(result);
            }
        }
        let mut batch = Batch::default();
        let d = batch.push(
            HUB_THREAD,
            None,
            caller.source(),
            EventBody::DecisionMade(DecisionMade {
                site: DecisionSite::Route,
                provider: DecisionProvider::User,
                options: vec![DELEGATE.to_string()],
                chosen: DELEGATE.to_string(),
                confidence: 1.0,
                latency_ms: 0,
                ..Default::default()
            }),
        );
        core.push_prompt(
            &mut batch,
            &orch,
            message,
            TurnFrom::User,
            caller.source(),
            Some(RoutedBy::Event(d)),
        )?;
        batch.user_send();
        cap_said_after(&mut batch, seen_cap(seen_gseq, core.gseq));
        let done = core.append_batch(&self.inner, batch).map_err(append_err)?;
        let queued = done.queued.first().copied().unwrap_or_default();
        let sent = core.dispatch_queued(&self.inner, &orch, queued);
        Ok(json!({
            "action": "delegate",
            "thread": orch,
            "thread_name": core.registry.get(&orch).map(|v| v.name.clone()),
            "ref": sent.msg_ref,
            "turn": sent.turn,
            "pending": sent.pending,
            "decision_ids": decision_ids(&done.events, 1),
        }))
    }

    /// Asks the user which thread gets `message`: the route `records` and the hub question
    /// holding the message (and what the user had seen, `seen_gseq`), in one batch.
    #[allow(clippy::too_many_arguments)]
    fn ask_user(
        &self,
        records: Vec<DecisionMade>,
        decided: bool,
        message: &str,
        options: Vec<String>,
        seen_gseq: Option<u64>,
        caller: &Caller,
        out: &mut Value,
    ) -> Result<(), RpcError> {
        let n = records.len();
        let asked_by = match caller {
            Caller::User(c) => Some(c.clone()),
            Caller::Orchestrator => None,
        };
        let attn_id = new_id();
        // The user may also drop the message.
        let mut options = options;
        if !options.iter().any(|o| o == CANCEL_OPTION) {
            options.push(CANCEL_OPTION.to_string());
        }
        let mut core = self.inner.core();
        if core.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let (mut batch, _) = route_records(records, decided);
        batch.push(
            HUB_THREAD,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: attn_id.clone(),
                reason: AttentionReason::Question,
                summary: format!("Which thread should get: {}?", preview(message)),
                options: options.clone(),
                ext: twr_ext_map(json!({
                    "pending_message": message,
                    "asked_by": asked_by,
                    "said_after": seen_cap(seen_gseq, core.gseq),
                })),
                ..Default::default()
            }),
        );
        let done = core.append_batch(&self.inner, batch).map_err(append_err)?;
        out["decision_ids"] = json!(decision_ids(&done.events, n));
        out["action"] = json!("ask");
        out["attn_id"] = json!(attn_id);
        out["options"] = json!(options);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_caps_said_after() {
        assert_eq!(seen_cap(None, 10), 0);
        assert_eq!(seen_cap(Some(4), 10), 4);
        assert_eq!(seen_cap(Some(40), 10), 10);
    }

    #[test]
    fn previews_are_one_short_line() {
        assert_eq!(preview("fix\n the   bug"), "fix the bug");
        let long = "word ".repeat(40);
        let p = preview(&long);
        assert!(
            p.chars().count() <= PREVIEW_CHARS && p.ends_with("..."),
            "{p}"
        );
    }

    #[test]
    fn pending_routes_come_from_ext() {
        let p = AttentionRaised {
            ext: twr_ext_map(json!({"pending_message": "deploy", "asked_by": "tui"})),
            ..Default::default()
        };
        assert_eq!(
            PendingRoute::from_raised(&p),
            Some(PendingRoute {
                message: "deploy".into(),
                asked_by: Some("tui".into()),
                said_after: None,
            })
        );
        let p = AttentionRaised {
            ext: twr_ext_map(json!({"pending_message": "deploy", "said_after": 7})),
            ..Default::default()
        };
        assert_eq!(PendingRoute::from_raised(&p).unwrap().said_after, Some(7));
        assert_eq!(PendingRoute::from_raised(&AttentionRaised::default()), None);
    }
}
