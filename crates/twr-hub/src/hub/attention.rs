//! The attention RPCs: `attention.list`, and `attention.respond` / `attention.dismiss`, which
//! record an answer only for the revision of the item it was given for and forward it to the
//! runtime that raised the item.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::adapter::Answer;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::AttentionItem;

use super::orch::Gate;
use super::runners::expect_recorded;
use super::supervisor::split_runtime_attn_id;
use super::{
    append_err, internal, route, rpc, store, twr_ext_map, AnswerFor, Batch, Caller, Core, Hub,
    Inner, Response, MAX_ID_BYTES,
};

impl Hub {
    /// `attention.list`: open attention items in stack order, each with its thread's `name`.
    /// Without `all`, only items that block a thread (permission, question).
    pub fn attention_list(&self, all: bool) -> Vec<Value> {
        let core = self.inner.core();
        core.attention
            .ordered(&HashSet::new())
            .into_iter()
            .filter(|i| all || i.reason.is_blocking())
            .map(|i| {
                let mut j = serde_json::to_value(i).unwrap_or(Value::Null);
                let name = core
                    .registry
                    .get(&i.thread)
                    .map_or(i.thread.clone(), |v| v.name.clone());
                j["thread_name"] = json!(name);
                j
            })
            .collect()
    }

    /// `attention.respond`: records the user's answer and forwards it to the runtime that
    /// raised the item. The hub never answers on its own; this is the only path that resolves
    /// an item with an answer.
    ///
    /// Without a revision (see [`Hub::respond_at`]) it answers only an item that was never
    /// raised again since it opened (`conflict` otherwise).
    pub fn respond(&self, attn_id: &str, answer: Answer, client: &str) -> Result<Event, RpcError> {
        self.respond_with(attn_id, Response::Answer(answer), &Caller::user(client))
    }

    /// [`Hub::respond_by`] for revision `revision` of the item (the control gseq of the
    /// `attention.raised` the client displayed, [`AttentionItem::revision`]; `None`: see
    /// [`Hub::respond`]). An item raised again since then (it may ask something else now) is
    /// not answered: `conflict`.
    ///
    /// [`AttentionItem::revision`]: twr_core::reduce::AttentionItem::revision
    pub fn respond_at(
        &self,
        attn_id: &str,
        answer: Answer,
        revision: Option<u64>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        applied(self.respond_relay(attn_id, answer, revision, caller)?)
    }

    /// [`Hub::respond_at`] for any caller: the orchestrator's answer is applied when the user's
    /// message is a command that covers it, and is otherwise a proposal for the user to
    /// confirm ([`Relay::NeedsConfirmation`]; the relay guard, see `hub::orch`).
    pub fn respond_relay(
        &self,
        attn_id: &str,
        answer: Answer,
        revision: Option<u64>,
        caller: &Caller,
    ) -> Result<Relay, RpcError> {
        self.respond_full(
            attn_id,
            Response::Answer(answer),
            caller,
            revision.map_or(AnswerFor::Unspecified, AnswerFor::Revision),
        )
    }

    /// [`Hub::respond_option_by`] for revision `revision` of the item (see [`Hub::respond_at`]).
    pub fn respond_option_at(
        &self,
        attn_id: &str,
        index: usize,
        message: Option<String>,
        revision: Option<u64>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        applied(self.respond_option_relay(attn_id, index, message, revision, caller)?)
    }

    /// [`Hub::respond_option_at`] for any caller (see [`Hub::respond_relay`]).
    pub fn respond_option_relay(
        &self,
        attn_id: &str,
        index: usize,
        message: Option<String>,
        revision: Option<u64>,
        caller: &Caller,
    ) -> Result<Relay, RpcError> {
        self.respond_full(
            attn_id,
            Response::Option { index, message },
            caller,
            revision.map_or(AnswerFor::Unspecified, AnswerFor::Revision),
        )
    }

    /// `attention.respond` with `option_index`: answers with the item's option at `index`
    /// (0-based), so a client never needs the option's full text. `message` is passed along.
    pub fn respond_option(
        &self,
        attn_id: &str,
        index: usize,
        message: Option<String>,
        client: &str,
    ) -> Result<Event, RpcError> {
        self.respond_with(
            attn_id,
            Response::Option { index, message },
            &Caller::user(client),
        )
    }

    /// [`Hub::respond`] for `caller`. The orchestrator is held to the relay guard (see
    /// `hub::orch`): an answer the user's message does not ask for as a command
    /// becomes a proposal, which this call reports as `conflict` (use [`Hub::respond_relay`]
    /// for the proposal), and it never resolves other items (`relay_forbidden`).
    pub fn respond_by(
        &self,
        attn_id: &str,
        answer: Answer,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        self.respond_with(attn_id, Response::Answer(answer), caller)
    }

    /// [`Hub::respond_option`] for `caller` (see [`Hub::respond_by`]).
    pub fn respond_option_by(
        &self,
        attn_id: &str,
        index: usize,
        message: Option<String>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        self.respond_with(attn_id, Response::Option { index, message }, caller)
    }

    fn respond_with(
        &self,
        attn_id: &str,
        response: Response,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        self.respond_seen(attn_id, response, caller, AnswerFor::Unspecified)
    }

    /// `attention.dismiss {attn_id, revision?}`: the user closes an item the hub raised (a
    /// `say` question, a confirm card, a review, an error, a stalled notice) without answering
    /// it: `attention.resolved{by: user, answer: cancel}`, and a question's message is routed
    /// nowhere (a confirm card applies nothing). A permission or question a runtime raised is
    /// refused (`bad_request`): the runtime waits for its answer, so it must be answered or
    /// the thread stopped. Only the user dismisses (the orchestrator gets `relay_forbidden`).
    /// `revision` as for [`Hub::respond_at`].
    pub fn attention_dismiss(
        &self,
        attn_id: &str,
        revision: Option<u64>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                "only the user dismisses attention items",
            ));
        }
        self.respond_seen(
            attn_id,
            Response::Dismiss,
            caller,
            revision.map_or(AnswerFor::Unspecified, AnswerFor::Revision),
        )
    }

    /// [`Hub::respond_with`], only while the item is still the revision the answer was given
    /// for (`basis`, see [`AnswerFor`]). Checked under the lock that records the answer, since
    /// the item can be raised again (asking something else) while a client shows the old
    /// card, or while `say` decides with no lock held.
    pub(super) fn respond_seen(
        &self,
        attn_id: &str,
        response: Response,
        caller: &Caller,
        basis: AnswerFor,
    ) -> Result<Event, RpcError> {
        applied(self.respond_full(attn_id, response, caller, basis)?)
    }

    /// [`Hub::respond_seen`] for any caller, under the core lock ([`Core::respond_locked`]).
    fn respond_full(
        &self,
        attn_id: &str,
        response: Response,
        caller: &Caller,
        basis: AnswerFor,
    ) -> Result<Relay, RpcError> {
        if attn_id.len() > MAX_ID_BYTES {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("attn_id is longer than {MAX_ID_BYTES} bytes"),
            ));
        }
        let mut core = self.inner.core();
        core.respond_locked(&self.inner, attn_id, response, caller, basis, None)
    }
}

/// An answer staged into a batch ([`Core::stage_answer`]): what to do once it commits
/// ([`Core::finish_answer`]).
pub(super) struct StagedAnswer {
    attn_id: String,
    thread: String,
    /// The runtime's own id of the item.
    runtime_id: String,
    answer: Answer,
    /// Whether a runtime waits for the answer.
    forward: bool,
    /// The orchestrator's answer (its per-turn allowance is used up).
    orchestrator: bool,
    /// Index of the item's `attention.resolved` among the batch's events.
    event: usize,
    /// The remote `respond` recorded with it.
    cmds: Vec<store::RemoteCmd>,
    /// For a hub question: the thread its message goes to and the index of that prompt
    /// among the batch's prompts.
    routed: Option<(String, usize)>,
}

/// What `attention.respond` did.
#[derive(Debug, Clone)]
pub enum Relay {
    /// The item was answered: its `attention.resolved`.
    Applied(Box<Event>),
    /// The user's message is no command asking for the orchestrator's answer: it was
    /// recorded as a proposal for the user to confirm on a card (`hub::proposal`). The value
    /// is the result the orchestrator gets (`status: "needs_confirmation"`).
    NeedsConfirmation(Value),
}

impl Relay {
    /// The RPC result: the event, or the proposal's result.
    pub fn into_json(self) -> Result<Value, RpcError> {
        match self {
            Relay::Applied(ev) => serde_json::to_value(ev).map_err(super::internal),
            Relay::NeedsConfirmation(v) => Ok(v),
        }
    }
}

/// The event of an applied answer; a proposal is `conflict` for callers that expect an event.
fn applied(r: Relay) -> Result<Event, RpcError> {
    match r {
        Relay::Applied(ev) => Ok(*ev),
        Relay::NeedsConfirmation(v) => Err(rpc(
            codes::CONFLICT,
            format!("the answer needs the user's confirmation: {v}"),
        )),
    }
}

impl Core {
    /// [`Hub::respond_full`] under the core lock. `proposal`: the confirm card whose Confirm
    /// applies this answer as the user's (recorded as `ext.twr.proposal`).
    pub(super) fn respond_locked(
        &mut self,
        hub: &Arc<Inner>,
        attn_id: &str,
        response: Response,
        caller: &Caller,
        basis: AnswerFor,
        proposal: Option<&str>,
    ) -> Result<Relay, RpcError> {
        // The item at its current revision: everything below judges (and records) this one.
        let item = self.open_item(attn_id)?;
        self.check_answer_basis(attn_id, &item, caller, basis)?;
        let dismiss = matches!(response, Response::Dismiss);
        if dismiss {
            self.refuse_runtime_dismiss(attn_id, &item)?;
        }
        // A delete card: only the user answers it, and Delete starts the deletion
        // (`hub::delete`).
        if let Some(card) = self.delete_cards.get(attn_id).cloned() {
            let decision = match response {
                Response::Dismiss => None,
                r => Some(answer_of(r, &item, attn_id)?.decision),
            };
            return self
                .answer_delete_card(hub, &item, &card, decision.as_deref(), caller)
                .map(|ev| Relay::Applied(Box::new(ev)));
        }
        // A disk card: only the user answers it, and Trim trims (`hub::disk`).
        if let Some(card) = self.disk.cards.get(attn_id).cloned() {
            let decision = match response {
                Response::Dismiss => None,
                r => Some(answer_of(r, &item, attn_id)?.decision),
            };
            return self
                .answer_disk_card(hub, attn_id, &card, decision.as_deref(), caller)
                .map(|ev| Relay::Applied(Box::new(ev)));
        }
        // A card about a prompt queued after other threads: only the user answers it, and
        // Send sends it (`hub::after`).
        if let Some(id) = self.after.cards.get(attn_id).cloned() {
            let decision = match response {
                Response::Dismiss => None,
                r => Some(answer_of(r, &item, attn_id)?.decision),
            };
            return self
                .answer_after_card(hub, attn_id, &id, decision.as_deref(), caller)
                .map(|ev| Relay::Applied(Box::new(ev)));
        }
        let card = self.hub_cards.get(attn_id).cloned();
        if card.is_some() && caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{attn_id} is a confirm card: only the user answers it (Confirm or \
                     Cancel); tell the user it is waiting for them"
                ),
            ));
        }
        let mut answer = answer_of(response, &item, attn_id)?;
        if let (Some(card), false) = (&card, dismiss) {
            if self.confirms(&item, &answer.decision)? {
                return self
                    .confirm_card(hub, &item, card, caller)
                    .map(|ev| Relay::Applied(Box::new(ev)));
            }
            answer = Answer::new(super::proposal::CANCEL);
        }
        if caller.is_orchestrator() {
            // Forward exactly the user's answer: the matched option label, nothing extra
            // (no message, `updatedInput` or other runtime fields of the orchestrator's making).
            let label = match self.relay_check(&item, &answer.decision)? {
                Gate::Direct(label) => label,
                Gate::Propose(label) => {
                    return Ok(Relay::NeedsConfirmation(self.propose_answer(&item, &label)));
                }
            };
            answer.decision = label;
            answer.extra = Default::default();
            answer.message = None;
        }
        let mut batch = Batch::default();
        let staged =
            self.stage_answer(hub, &mut batch, &item, answer, dismiss, caller, proposal)?;
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        let ev = done
            .events
            .get(staged.event)
            .cloned()
            .ok_or_else(|| internal("the answer was not recorded"))?;
        self.finish_answer(hub, &staged, &done.queued);
        Ok(Relay::Applied(Box::new(ev)))
    }

    /// Adds to `batch` the answer `answer` to `item` (validated by the caller as an answer to
    /// its current revision): checks what recording it needs (the hub question's routing, the
    /// runtime that waits for it, the revision still current), then pushes its
    /// `attention.resolved` (`ext.twr.proposal` = the confirm card `proposal` that applies
    /// it), and for a hub question the routing decision and the queued message, for a remote
    /// runtime the `respond` command. What happens once the batch commits is
    /// [`Core::finish_answer`]'s. On an error `batch` is left as it was.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stage_answer(
        &mut self,
        hub: &Arc<Inner>,
        batch: &mut Batch,
        item: &AttentionItem,
        answer: Answer,
        dismiss: bool,
        caller: &Caller,
        proposal: Option<&str>,
    ) -> Result<StagedAnswer, RpcError> {
        let attn_id = item.attn_id.as_str();
        let revision = item.revision;
        let routing =
            self.hub_question_routing(hub, attn_id, item, &answer.decision, dismiss, caller)?;
        let forward = self.adapter_attn.contains_key(attn_id);
        // The runtime gets its own id back (see `namespace_attention`). An item recorded
        // before ids were namespaced keeps its id as it is.
        let (item_gen, runtime_id) =
            split_runtime_attn_id(&item.thread, attn_id).unwrap_or((None, attn_id));
        if forward {
            self.check_answer_runtime(attn_id, item, item_gen)?;
        }
        // What was validated above is still what the item asks (nothing above releases the
        // lock, so this holds; it is checked here, next to the record, so it keeps holding).
        if self.attention.get(attn_id).map(|i| i.revision) != Some(revision) {
            return Err(rpc(
                codes::CONFLICT,
                format!("the question changed; look again: {attn_id} was raised again"),
            ));
        }
        let by = if caller.is_orchestrator() {
            ResolvedBy::Orchestrator
        } else {
            ResolvedBy::User
        };
        let resolved = EventBody::AttentionResolved(AttentionResolved {
            attn_id: attn_id.into(),
            by,
            answer: Some(answer.clone()),
            ext: proposal.and_then(|card| twr_ext_map(json!({ PROPOSAL_KEY: card }))),
            ..Default::default()
        });
        let event = batch.events.len();
        let mut staged = StagedAnswer {
            attn_id: attn_id.to_string(),
            thread: item.thread.clone(),
            runtime_id: runtime_id.to_string(),
            answer: answer.clone(),
            forward,
            orchestrator: caller.is_orchestrator(),
            event,
            cmds: Vec::new(),
            routed: None,
        };
        match routing {
            // The answer, its routing decision and the queued message go together.
            Some((thread, pending)) => {
                let prompt = self.stage_hub_answer(
                    batch,
                    item,
                    resolved,
                    &answer.decision,
                    &pending,
                    &thread,
                    caller,
                )?;
                staged.routed = Some((thread, prompt));
            }
            None => {
                let turn = self.registry.get(&item.thread).and_then(|v| v.current_turn);
                // The answer to a remote runtime is recorded with it (`hub::runners`).
                staged.cmds = forward
                    .then(|| self.remote_respond(hub, &item.thread, turn, runtime_id, &answer))
                    .flatten()
                    .into_iter()
                    .collect();
                batch.push(&item.thread, turn, caller.source(), resolved);
                batch.remote_cmds.extend(staged.cmds.iter().cloned());
            }
        }
        Ok(staged)
    }

    /// What follows an answer [`Core::stage_answer`] staged, once its batch committed
    /// (`queued`: the batch's queue rows): [`Core::forward_answer`], then
    /// [`Core::after_answer`].
    pub(super) fn finish_answer(
        &mut self,
        hub: &Arc<Inner>,
        staged: &StagedAnswer,
        queued: &[i64],
    ) {
        self.forward_answer(hub, staged);
        self.after_answer(hub, staged, queued);
    }

    /// The first half of what follows a committed answer: the remote box's forwarder is told
    /// of its `respond` and the runtime gets the answer. Nothing is dispatched or restarted
    /// here, so a confirm card forwards every answer it staged before any restart
    /// (`hub::proposal`).
    pub(super) fn forward_answer(&mut self, hub: &Arc<Inner>, staged: &StagedAnswer) {
        for c in &staged.cmds {
            expect_recorded(hub, c);
        }
        if staged.orchestrator {
            self.orch.consume_answer(&staged.attn_id);
        }
        // Forwarding is not an append: a runtime that dies before it gets the answer is
        // closed like any other (its items resolve `thread_stopped`).
        let mut forwarded = false;
        if staged.forward {
            if let Some(live) = self.rt.get(&staged.thread).and_then(|r| r.live.as_ref()) {
                forwarded = live
                    .handle
                    .respond(&staged.runtime_id, staged.answer.clone())
                    .is_ok();
                if !forwarded {
                    tracing::warn!(attn_id = %staged.attn_id, "runtime exited before receiving the answer");
                }
            }
        }
        // No forwarder will issue it: its expected id must not be handed to a later answer
        // to the same attention id (the record itself is settled with the session).
        if !forwarded {
            for c in &staged.cmds {
                super::runners::forget_recorded(hub, c);
            }
        }
    }

    /// The second half, once the answer was forwarded ([`Core::forward_answer`]): the routed
    /// message is dispatched, and a settings restart the item deferred (`hub::restart`) may go
    /// ahead (after the answer: the runtime gets it before the stop).
    pub(super) fn after_answer(&mut self, hub: &Arc<Inner>, staged: &StagedAnswer, queued: &[i64]) {
        if let Some((thread, prompt)) = &staged.routed {
            if let Some(&row) = queued.get(*prompt) {
                self.dispatch_queued(hub, thread, row);
            }
        }
        let thread = &staged.thread;
        if staged.forward && self.restart_due(thread) {
            if let Err(e) = self.try_dispatch(hub, thread) {
                tracing::error!(thread = %thread, error = %format!("{e:#}"), "dispatching after an answer failed");
            }
        }
    }

    /// The open item `attn_id`: `conflict` when it is already resolved, `not_found` when there
    /// never was one.
    pub(super) fn open_item(&self, attn_id: &str) -> Result<AttentionItem, RpcError> {
        let Some(item) = self.attention.get(attn_id).cloned() else {
            return Err(if let Some(outcome) = self.card_outcomes.get(attn_id) {
                rpc(
                    codes::CONFLICT,
                    format!("confirm card {attn_id} is already resolved: {outcome}"),
                )
            } else if self.known_attn.contains(attn_id) {
                rpc(
                    codes::CONFLICT,
                    format!("attention {attn_id} is already resolved"),
                )
            } else {
                rpc(codes::NOT_FOUND, format!("no attention item {attn_id}"))
            });
        };
        Ok(item)
    }

    /// Whether an answer given for `basis` may answer `item` (`attn_id`) as it is now, at its
    /// current revision: `conflict` when it was raised again since (see [`AnswerFor`]). The
    /// orchestrator's answer without a revision is judged by the relay guard instead.
    pub(super) fn check_answer_basis(
        &self,
        attn_id: &str,
        item: &AttentionItem,
        caller: &Caller,
        basis: AnswerFor,
    ) -> Result<(), RpcError> {
        let revision = item.revision;
        match basis {
            AnswerFor::SeenBy(seen) => {
                if !self.orch.raised_gseq(attn_id).is_some_and(|r| r <= seen) {
                    return Err(rpc(
                        codes::CONFLICT,
                        format!(
                            "{attn_id} was raised again after the message was written (it may \
                             ask something else now); answer it again"
                        ),
                    ));
                }
            }
            AnswerFor::Revision(shown) if shown != revision => {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "the question changed; look again: {attn_id} was raised again since \
                         revision {shown} (it may ask something else now; it is {attn_id}@\
                         {revision})"
                    ),
                ));
            }
            AnswerFor::Revision(_) => {}
            // The orchestrator's answer is judged by the relay guard below (the user's message
            // must postdate the current revision).
            AnswerFor::Unspecified if caller.is_orchestrator() => {}
            AnswerFor::Unspecified if revision != item.first_revision => {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "the question changed; look again: {attn_id} was raised again since it \
                         was first asked (it may ask something else now); answer the current \
                         revision as {attn_id}@{revision}"
                    ),
                ));
            }
            AnswerFor::Unspecified => {}
        }
        Ok(())
    }

    /// Refuses to dismiss `item` (`attn_id`) when a runtime raised it (`bad_request`): the
    /// runtime waits for its answer, so it must be answered or the thread stopped.
    fn refuse_runtime_dismiss(&self, attn_id: &str, item: &AttentionItem) -> Result<(), RpcError> {
        if self.adapter_attn.contains_key(attn_id)
            || split_runtime_attn_id(&item.thread, attn_id).is_some()
        {
            let revision = item.revision;
            let name = self
                .registry
                .get(&item.thread)
                .map_or(item.thread.clone(), |v| v.name.clone());
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{attn_id} was raised by {name}'s runtime, which waits for the answer: \
                     answer it (`twr respond {attn_id}@{revision} <answer>`) or stop the \
                     thread (`twr stop {name}`); only items the hub raised can be dismissed"
                ),
            ));
        }
        Ok(())
    }

    /// Where the answer `decision` to hub question `attn_id` (`item`) routes the question's
    /// message: the target thread (created for the orchestrator if needed) with the pending
    /// message. `None` for an item that is no hub question, or a question cancelled (or
    /// `dismiss`ed): its message goes nowhere. Everything that can refuse the answer happens
    /// here, before it is recorded, so the question never closes without its message going
    /// somewhere.
    fn hub_question_routing(
        &mut self,
        hub: &Inner,
        attn_id: &str,
        item: &AttentionItem,
        decision: &str,
        dismiss: bool,
        caller: &Caller,
    ) -> Result<Option<(String, route::PendingRoute)>, RpcError> {
        Ok(match self.hub_questions.get(attn_id).cloned() {
            // Cancelled: the message goes nowhere.
            Some(_) if dismiss || route::cancels(item, decision) => None,
            Some(pending) => {
                let target = self.hub_answer_target(hub, item, decision, caller)?;
                let thread = self.hub_answer_thread(hub, target)?;
                Some((thread, pending))
            }
            None => None,
        })
    }

    /// Whether the runtime that raised `item` (`attn_id`, of runtime generation `item_gen`)
    /// can be given the answer now: it is the thread's current runtime, its box is reachable,
    /// and it runs (`conflict` / `box_offline` otherwise).
    fn check_answer_runtime(
        &self,
        attn_id: &str,
        item: &AttentionItem,
        item_gen: Option<u64>,
    ) -> Result<(), RpcError> {
        if item_gen.is_some_and(|g| g != self.runtime_gen(&item.thread)) {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{attn_id} was raised by an earlier runtime of the thread; it no longer \
                     waits for an answer"
                ),
            ));
        }
        // The answer goes to a runtime on the item's box: it must be reachable now (a box
        // being drained takes it while its turns may still finish, `hub::drain`).
        if let Some(view) = self.registry.get(&item.thread).cloned() {
            self.require_box_for_answer(&view)?;
        }
        let alive = self
            .rt
            .get(&item.thread)
            .and_then(|r| r.live.as_ref())
            .is_some_and(|l| !l.handle.commander().is_closed());
        if !alive {
            return Err(rpc(
                codes::CONFLICT,
                format!("the runtime that raised {attn_id} is not running"),
            ));
        }
        Ok(())
    }
}

/// The answer `response` gives to `item` (`attn_id`): a dismissal is [`route::CANCEL`], an
/// option index the option's label (`bad_request` when out of range). A permission is only
/// answered with one of its options.
pub(super) fn answer_of(
    response: Response,
    item: &AttentionItem,
    attn_id: &str,
) -> Result<Answer, RpcError> {
    let dismiss = matches!(response, Response::Dismiss);
    let answer = match response {
        Response::Dismiss => Answer::new(route::CANCEL),
        Response::Answer(a) => a,
        Response::Option { index, message } => match item.options.get(index) {
            Some(label) => Answer {
                decision: label.clone(),
                message,
                ..Default::default()
            },
            None => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "option_index {index} is out of range: {attn_id} has {} options",
                        item.options.len()
                    ),
                ))
            }
        },
    };
    if item.reason == AttentionReason::Permission
        && !dismiss
        && !item.options.is_empty()
        && !item.options.contains(&answer.decision)
    {
        return Err(rpc(
            codes::BAD_REQUEST,
            format!(
                "answer {:?} is not one of {:?}",
                answer.decision, item.options
            ),
        ));
    }
    Ok(answer)
}
