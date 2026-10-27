//! A worker's context whose prompt cache expired (`[threads] idle_compaction`).
//!
//! Claude Code caches the prompt of each model call for a while (an hour on a subscription,
//! five minutes with an API key): the next call within that time reads the context from the
//! cache, a later one reads all of it again, uncached. The thread knows when its session's
//! cache expires (`ThreadView::cache_expires_at`, from its turns' ends) and how large its
//! context is (`ThreadView::context`).
//!
//! **The offer** (`ask`, the default, and `before_expiry`). When the next queued message of a
//! worker thread is the user's or the orchestrator's, its cache expired, its context is known
//! and holds [`IDLE_COMPACTION_MIN_TOKENS`] or more, and the thread could compact now
//! (`Core::compaction_refusal`: a local box, a runtime that compacts from twr, a confirmed
//! session, not parked or attached; no compaction asked for or running), the message is not
//! started: a card on the hub's thread (`attention.raised`, `question`, with
//! `ext.twr.idle_offer = {thread, session, expires_at, tokens}`) asks the user whether to
//! compact the context first, and the thread's queue waits for the answer. Its text says the
//! thread's name, the context's size and how long ago the cache was written and expired;
//! nothing of the worker's. A prompt of the hub's (a job's wake, a wake after background work,
//! a notice) raises no card: an unattended flow does not wait on one. The queue keeps its
//! order all the same: one queued behind a held message waits with it. The offer is not made
//! for the orchestrator (its compaction is the user's own conversation), for a thread whose
//! offers the user turned off (`ThreadView::idle_compaction_off`), nor under `off`.
//!
//! Only the user answers the card (the orchestrator gets `relay_forbidden`); the answer and
//! what it does are one batch, then the queue goes on. Their own `thread.compact` of the
//! thread while it is open answers it `Compact first` (`hub::compact`: the compaction is that
//! ask's, no `compact_by`), and their `message.send_now` of the held message `Send as is`
//! (`hub::queue`), each recorded as their answer:
//!
//! - [`COMPACT_FIRST`]: a compaction (`hub::compact`, `ext.twr.compact_by: "offer"`, the
//!   user's) runs before the held messages. One that can no longer run (the thread moved,
//!   was parked, ...) is not asked for: a `conversation.update` on the thread says why, and
//!   the messages go as they are.
//! - [`SEND_AS_IS`], or a dismissal: the messages go as they are.
//! - `Don't ask for <name>` ([`DONT_ASK_PREFIX`]): as `Send as is`, and the thread's offers are
//!   turned off (`conversation.update` with `ext.twr.idle_compaction: "off"`, the user's;
//!   `thread.compact {offers: "on"}` turns them back on).
//!
//! Whatever the answer, a queue a failure held (`ThreadRt::queue_held`) is let go, as the
//! user's own send lets it go, and the hub does not ask again for the same session and expiry
//! (memory only: a hub restart may ask once more). The cards are folded from the log
//! ([`Core::track_idle_offer`]), so a card stays open, and its thread's queue held, across a
//! hub restart. A card that no longer applies when the queue is next looked at (its thread
//! moved to another runtime or session, a turn ran meanwhile and the cache is another, the
//! held message was taken back and no message of the user's or the orchestrator's is next,
//! offers turned off, ...) is closed by the hub (`by: timeout`, no answer) and the queue goes
//! on. Besides each dispatch, the queue is looked at for an open card when a move with no
//! runtime is recorded (`hub::restart`), when the held message is taken back
//! (`hub::withdraw`), and when a runtime reports its session (`thread.started` /
//! `thread.resumed`, `hub::supervisor`, `hub::runners`: a runtime restarted with no turn may
//! be on another session). Stopping or deleting the thread closes it (`by: thread_stopped`).
//!
//! **Compaction before expiry** (`before_expiry` only, which the user configures: the one case
//! where the hub compacts a context on its own). Every [`IDLE_COMPACTION_TICK`], from once its
//! startup recovery is done (the previous hub's runtimes killed, their threads closed), the hub
//! looks at each worker that could get an offer (but for the message) and is idle: no turn,
//! nothing queued, no blocking item, no background task of its runtime, not stopped or crashed,
//! its latest turn not a compaction (one per cache: a compaction leaves a small context, and
//! one that failed is not tried again). One whose cache lives an hour or more and expires
//! within [`IDLE_COMPACTION_LEAD`] but not within [`IDLE_COMPACTION_MARGIN`] is compacted
//! (`source: hub`, `ext.twr.compact_by: "idle"`). A five-minute cache never is: it would be
//! compacted after nearly every turn.

use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use twr_core::config::IdleCompaction;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::AttentionItem;

use super::compact::{CompactAsk, CompactBy};
use super::store::PendingPrompt;
use super::{
    append_err, internal, rpc, twr_ext, twr_ext_map, Batch, Caller, Core, HubState, Inner,
};

/// Smallest context, in tokens, the hub offers to compact (or compacts before expiry).
pub const IDLE_COMPACTION_MIN_TOKENS: u64 = 200_000;
/// The card's option that compacts the context before the held messages.
pub const COMPACT_FIRST: &str = "Compact first";
/// The card's option that sends them as they are.
pub const SEND_AS_IS: &str = "Send as is";
/// The card's third option, followed by the thread's name: sends them as they are and turns
/// the thread's offers off.
pub const DONT_ASK_PREFIX: &str = "Don't ask for";
/// How often the hub looks for an idle worker to compact before its cache expires.
pub const IDLE_COMPACTION_TICK: Duration = Duration::from_secs(60);
/// How long before a cache expires the hub may compact it.
pub const IDLE_COMPACTION_LEAD: Duration = Duration::from_secs(5 * 60);
/// How long before a cache expires the hub no longer starts a compaction: one started later
/// could reach the runtime after it expired.
pub const IDLE_COMPACTION_MARGIN: Duration = Duration::from_secs(30);
/// Shortest cache, in seconds, the hub compacts before it expires.
pub const IDLE_COMPACTION_MIN_TTL_S: u64 = 3600;

/// `attention.raised` `ext.twr` key of an offer card ([`IdleOffer`]).
pub(super) const IDLE_OFFER_KEY: &str = "idle_offer";

/// What an offer card is about (`ext.twr.idle_offer`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct IdleOffer {
    /// The worker thread.
    pub(super) thread: String,
    /// Its session then.
    pub(super) session: String,
    /// When that session's prompt cache expired.
    pub(super) expires_at: DateTime<Utc>,
    /// How large its context was, in tokens.
    pub(super) tokens: u64,
}

/// An answer to an offer card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    CompactFirst,
    SendAsIs,
    DontAsk,
}

impl Core {
    /// Folds `ev` (live and on replay): the open offer cards.
    pub(super) fn track_idle_offer(&mut self, ev: &Event) {
        match &ev.body {
            EventBody::AttentionRaised(p)
                if ev.thread == HUB_THREAD && ev.source == Source::Hub =>
            {
                if let Some(offer) = twr_ext(&p.ext)
                    .and_then(|t| t.get(IDLE_OFFER_KEY))
                    .and_then(|o| serde_json::from_value::<IdleOffer>(o.clone()).ok())
                {
                    self.idle_offers.insert(p.attn_id.clone(), offer);
                }
            }
            EventBody::AttentionResolved(p) => {
                self.idle_offers.remove(&p.attn_id);
            }
            _ => {}
        }
    }

    /// What an offer for thread `thread` would be about now, but for the message and the
    /// time: the mode allows it, it is a worker whose offers are on, with a confirmed session,
    /// a known context of [`IDLE_COMPACTION_MIN_TOKENS`] or more and a known cache expiry, that
    /// could compact now, with no compaction asked for or running.
    fn idle_offer_basis(&self, hub: &Inner, thread: &str) -> Option<IdleOffer> {
        if hub.config.threads.idle_compaction == IdleCompaction::Off
            || thread == HUB_THREAD
            || self.orch.is_thread(thread)
        {
            return None;
        }
        let view = self.registry.get(thread)?;
        if view.is_orchestrator() || view.idle_compaction_off {
            return None;
        }
        let session = view.confirmed_session()?.to_string();
        let tokens = view
            .context
            .map(|c| c.tokens)
            .filter(|t| *t >= IDLE_COMPACTION_MIN_TOKENS)?;
        let expires_at = view.cache_expires_at?;
        if view.current_turn_compact || self.rt.get(thread).is_some_and(|r| r.compact.is_some()) {
            return None;
        }
        if !matches!(self.compaction_refusal(hub, thread), Ok(None)) {
            return None;
        }
        Some(IdleOffer {
            thread: thread.to_string(),
            session,
            expires_at,
            tokens,
        })
    }

    /// The offer thread `thread` is due at `now` before queued message `p` starts: `p` is the
    /// user's or the orchestrator's, the cache expired, and the user did not answer an offer
    /// for this session and expiry already ([`super::ThreadRt::offer_waived`]).
    fn idle_offer_due(
        &self,
        hub: &Inner,
        thread: &str,
        p: &PendingPrompt,
        now: DateTime<Utc>,
    ) -> Option<IdleOffer> {
        if !matches!(p.source, Source::User(_) | Source::Orchestrator) {
            return None;
        }
        let offer = self.idle_offer_basis(hub, thread)?;
        if now < offer.expires_at {
            return None;
        }
        let waived = self
            .rt
            .get(thread)
            .and_then(|r| r.offer_waived.as_ref())
            .is_some_and(|(s, at)| *s == offer.session && *at == offer.expires_at);
        (!waived).then_some(offer)
    }

    /// Thread `thread`'s open offer card, if any: its attention id and what it is about.
    fn idle_offer_card(&self, thread: &str) -> Option<(String, IdleOffer)> {
        let mut open: Vec<(&String, &IdleOffer)> = self
            .idle_offers
            .iter()
            .filter(|(_, o)| o.thread == thread)
            .collect();
        open.sort_by(|a, b| a.0.cmp(b.0));
        open.first().map(|(a, o)| ((*a).clone(), (*o).clone()))
    }

    /// Whether the start of thread `thread`'s next queued message `next` waits for an offer
    /// card ([`Core::dispatch_next`]): its open card still applies, or one is due and is
    /// raised now. An open card that no longer applies is closed. A card that cannot be
    /// recorded holds nothing: the message goes.
    pub(super) fn idle_offer_holds(
        &mut self,
        hub: &Inner,
        thread: &str,
        next: Option<&PendingPrompt>,
    ) -> bool {
        let open = self.idle_offer_card(thread);
        if open.is_none() && next.is_none() {
            return false;
        }
        let now = Utc::now();
        let due = next.and_then(|p| self.idle_offer_due(hub, thread, p, now));
        if let (Some((_, card)), Some(offer)) = (&open, &due) {
            if card.session == offer.session && card.expires_at == offer.expires_at {
                return true;
            }
        }
        if open.is_none() && due.is_none() {
            return false;
        }
        let mut batch = Batch::default();
        if let Some((attn_id, _)) = &open {
            batch.push(
                HUB_THREAD,
                None,
                Source::Hub,
                offer_closed(attn_id, ResolvedBy::Timeout),
            );
        }
        if let Some(offer) = &due {
            batch.push(HUB_THREAD, None, Source::Hub, self.offer_card(offer, now));
        }
        match self.append_batch(hub, batch) {
            Ok(_) => due.is_some(),
            Err(e) => {
                tracing::warn!(thread, error = %format!("{e:#}"), "cannot record the offer to compact a context first; the message goes as it is");
                false
            }
        }
    }

    /// The card offering to compact `offer`'s context before its messages go, at `now`.
    fn offer_card(&self, offer: &IdleOffer, now: DateTime<Utc>) -> EventBody {
        let name = self.thread_name(&offer.thread);
        let written = self
            .registry
            .get(&offer.thread)
            .and_then(|v| v.cache_ttl_s)
            .and_then(|t| i64::try_from(t).ok())
            .and_then(chrono::Duration::try_seconds)
            .map(|ttl| offer.expires_at - ttl);
        let idle = match written {
            Some(at) => format!(
                "idle {}; the cache expired {} ago",
                span(now - at),
                span(now - offer.expires_at)
            ),
            None => format!("the cache expired {} ago", span(now - offer.expires_at)),
        };
        let summary = format!(
            "{name}: its context ({}k tokens) is no longer cached ({idle}). Compact it before \
             sending? Compacting reads it once more and summarizes it; sending as is reads all \
             of it uncached.",
            (offer.tokens + 500) / 1000
        );
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: new_id(),
            reason: AttentionReason::Question,
            summary,
            options: vec![
                COMPACT_FIRST.into(),
                SEND_AS_IS.into(),
                format!("{DONT_ASK_PREFIX} {name}"),
            ],
            ext: twr_ext_map(json!({ IDLE_OFFER_KEY: offer })),
            ..Default::default()
        })
    }

    /// Stages, in `batch`, the closing of thread `thread`'s open offer cards (it is being
    /// stopped or deleted: its queue goes).
    pub(super) fn stage_idle_offer_close(&self, batch: &mut Batch, thread: &str) {
        let mut open: Vec<&String> = self
            .idle_offers
            .iter()
            .filter(|(a, o)| o.thread == thread && self.attention.get(a).is_some())
            .map(|(a, _)| a)
            .collect();
        open.sort();
        for attn_id in open {
            batch.push(
                HUB_THREAD,
                None,
                Source::Hub,
                offer_closed(attn_id, ResolvedBy::ThreadStopped),
            );
        }
    }

    /// Whether `attn_id` is an open offer card, and what it is about.
    pub(super) fn idle_offer(&self, attn_id: &str) -> Option<IdleOffer> {
        self.idle_offers.get(attn_id).cloned()
    }

    /// Answers offer card `item` (about `offer`; the user only): see the module docs. The
    /// answer and what it records are one batch; the thread's queue is dispatched after it.
    pub(super) fn answer_idle_offer(
        &mut self,
        hub: &Arc<Inner>,
        item: &AttentionItem,
        offer: &IdleOffer,
        decision: Option<&str>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        let attn_id = item.attn_id.as_str();
        let thread = offer.thread.as_str();
        let name = self.thread_name(thread);
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{attn_id} asks the user whether to compact {name}'s context before \
                     sending to it: only the user answers it; tell the user it is waiting for \
                     them"
                ),
            ));
        }
        let dont_ask = item
            .options
            .get(2)
            .cloned()
            .unwrap_or_else(|| format!("{DONT_ASK_PREFIX} {name}"));
        let is = |d: &str, label: &str| d.eq_ignore_ascii_case(label);
        let choice = match decision.map(str::trim) {
            None => Choice::SendAsIs,
            Some(d) if is(d, COMPACT_FIRST) => Choice::CompactFirst,
            Some(d) if is(d, SEND_AS_IS) => Choice::SendAsIs,
            Some(d) if is(d, &dont_ask) => Choice::DontAsk,
            Some(other) => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "{other:?} is not an answer to {attn_id}: answer {COMPACT_FIRST:?}, \
                         {SEND_AS_IS:?} or {dont_ask:?}"
                    ),
                ))
            }
        };
        let (answered, _) = self.settle_idle_offer(
            hub,
            attn_id,
            offer,
            choice,
            decision,
            CompactBy::Offer,
            caller,
        )?;
        if self.registry.get(thread).is_some() {
            if let Err(e) = self.try_dispatch(hub, thread) {
                tracing::warn!(thread, error = %format!("{e:#}"), "dispatching after an offer to compact was answered failed");
            }
        }
        Ok(answered)
    }

    /// Records the user's `choice` on offer card `attn_id` (about `offer`), with `answer` as
    /// the card's recorded answer (none: a dismissal), and what it does, in one batch: a
    /// `Compact first` asks for a compaction (`by`: [`CompactBy::Offer`] for the card's own
    /// answer, [`CompactBy::User`] for the user's `thread.compact`) before the messages it
    /// held, `Don't ask` turns the thread's offers off. Whatever the answer, a queue a failure
    /// held is let go, as the user's own send lets it go (`ThreadRt::queue_held`). Dispatches
    /// nothing: the caller does. Returns the card's resolution and whether a compaction was
    /// asked for.
    #[allow(clippy::too_many_arguments)]
    fn settle_idle_offer(
        &mut self,
        hub: &Arc<Inner>,
        attn_id: &str,
        offer: &IdleOffer,
        choice: Choice,
        answer: Option<&str>,
        by: CompactBy,
        caller: &Caller,
    ) -> Result<(Event, bool), RpcError> {
        let thread = offer.thread.as_str();
        let name = self.thread_name(thread);
        let mut batch = Batch::default();
        batch.push(
            HUB_THREAD,
            None,
            caller.source(),
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: attn_id.to_string(),
                by: ResolvedBy::User,
                answer: answer.map(twr_core::adapter::Answer::new),
                ..Default::default()
            }),
        );
        let view = self.registry.get(thread).cloned();
        // Still the session the card asked about (a thread deleted or moved since: the answer
        // only closes it).
        let current = view
            .as_ref()
            .is_some_and(|v| v.confirmed_session() == Some(offer.session.as_str()));
        let mut ask: Option<CompactAsk> = None;
        match (choice, &view) {
            (Choice::CompactFirst, Some(v)) if current => {
                let refused = match self.compaction_refusal(hub, thread) {
                    Ok(r) => r,
                    Err(e) => Some(e.message),
                };
                match refused {
                    None => {
                        // Before the messages it held: one row before the first of them.
                        let next = self.store.next_pending(thread).map_err(internal)?;
                        ask = Some(CompactAsk {
                            after: next.map_or(0, |p| p.id - 1),
                            source: caller.source(),
                            runtime: v.runtime.clone(),
                            session: v.session_id.clone(),
                            by,
                        });
                    }
                    Some(why) => {
                        batch.push(
                            thread,
                            None,
                            Source::Hub,
                            EventBody::ConversationUpdate(ConversationUpdate {
                                text: format!(
                                    "Did not compact {name}'s context first: {why}. Its \
                                     messages go as they are."
                                ),
                                ..Default::default()
                            }),
                        );
                    }
                }
            }
            (Choice::DontAsk, Some(_)) => {
                batch.push(thread, None, caller.source(), offers_update(&name, true));
            }
            _ => {}
        }
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        let mut compacts = false;
        if let Some(rt) = self.rt.get_mut(thread).filter(|_| current) {
            // Not asked again for this cache, whatever the answer (a compaction that fails
            // leaves it as it was).
            rt.offer_waived = Some((offer.session.clone(), offer.expires_at));
            if let Some(ask) = ask {
                rt.compact = Some(ask);
                compacts = true;
            }
        }
        // The user's answer, like their send, lets a queue a failure held go: the card closed,
        // nothing else would.
        self.release_queue(thread);
        let answered = done
            .events
            .into_iter()
            .find(|e| matches!(&e.body, EventBody::AttentionResolved(r) if r.attn_id == attn_id))
            .ok_or_else(|| internal("the answer was not recorded"))?;
        Ok((answered, compacts))
    }

    /// The user's `thread.compact` of thread `thread` while an offer card holds its queue:
    /// answers the card `Compact first`, theirs, so the compaction runs before the messages it
    /// held (not after them, as a compaction asked for behind a queue would). Whether a
    /// compaction was asked for (no card, or one about a session the thread no longer has: no).
    pub(super) fn compact_past_idle_offer(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        caller: &Caller,
    ) -> Result<bool, RpcError> {
        let Some((attn_id, offer)) = self.idle_offer_card(thread) else {
            return Ok(false);
        };
        let (_, compacts) = self.settle_idle_offer(
            hub,
            &attn_id,
            &offer,
            Choice::CompactFirst,
            Some(COMPACT_FIRST),
            CompactBy::User,
            caller,
        )?;
        Ok(compacts)
    }

    /// The user's `message.send_now` of thread `thread`'s next queued message: an offer card
    /// that holds it is answered `Send as is`, theirs (the message goes as it is).
    pub(super) fn send_past_idle_offer(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        caller: &Caller,
    ) -> Result<(), RpcError> {
        if let Some((attn_id, offer)) = self.idle_offer_card(thread) {
            self.settle_idle_offer(
                hub,
                &attn_id,
                &offer,
                Choice::SendAsIs,
                Some(SEND_AS_IS),
                CompactBy::Offer,
                caller,
            )?;
        }
        Ok(())
    }

    /// Thread `thread`'s open offer card's attention id, if one holds its queue.
    pub(super) fn idle_offer_waiting(&self, thread: &str) -> Option<String> {
        self.idle_offer_card(thread).map(|(a, _)| a)
    }

    /// `thread.compact {thread, offers}`: turns thread `id`'s offers on or off (`off`), as
    /// `caller` asked (the user). Returns `{thread, thread_name, offers}`.
    pub(super) fn set_idle_offers(
        &mut self,
        hub: &Arc<Inner>,
        id: &str,
        off: bool,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        self.refuse_retired(id)?;
        self.refuse_deleting(id)?;
        let view = self.view(id)?;
        if view.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "the orchestrator gets no offers to compact its context: they are for worker \
                 threads",
            ));
        }
        let name = view.name.clone();
        self.append(hub, id, None, caller.source(), offers_update(&name, off))
            .map_err(append_err)?;
        // A card it holds the queue on closes at the next look, when this turned them off.
        if self.idle_offer_card(id).is_some() {
            if let Err(e) = self.try_dispatch(hub, id) {
                tracing::warn!(thread = %id, error = %format!("{e:#}"), "dispatching after the offers were turned off failed");
            }
        }
        Ok(json!({
            "thread": id,
            "thread_name": name,
            "offers": if off { "off" } else { "on" },
        }))
    }

    /// Thread `id` after something that may make its open offer card stale (its held message
    /// taken back, a move with no runtime, a session reported): a card that no longer applies
    /// is closed, and the queue goes on.
    pub(super) fn recheck_idle_offer(&mut self, hub: &Arc<Inner>, id: &str) {
        if self.idle_offer_card(id).is_some() {
            if let Err(e) = self.try_dispatch(hub, id) {
                tracing::warn!(thread = %id, error = %format!("{e:#}"), "dispatching for an open offer to compact failed");
            }
        }
    }

    /// One round of [`idle_compaction_ticker`] at `now`: under `before_expiry`, compacts each
    /// idle worker whose cache expires soon (see the module docs).
    fn compact_before_expiry(&mut self, hub: &Arc<Inner>, now: DateTime<Utc>) {
        if hub.config.threads.idle_compaction != IdleCompaction::BeforeExpiry || self.shutting_down
        {
            return;
        }
        let ids: Vec<String> = self.registry.list().iter().map(|v| v.id.clone()).collect();
        for id in ids {
            let Some(offer) = self.idle_compaction_due(hub, &id, now) else {
                continue;
            };
            let Some(view) = self.registry.get(&id) else {
                continue;
            };
            let ask = CompactAsk {
                after: 0,
                source: Source::Hub,
                runtime: view.runtime.clone(),
                session: Some(offer.session.clone()),
                by: CompactBy::Idle,
            };
            tracing::info!(thread = %id, tokens = offer.tokens, expires_at = %offer.expires_at, "compacting an idle worker's context before its prompt cache expires");
            self.rt(&id).compact = Some(ask);
            if let Err(e) = self.try_dispatch(hub, &id) {
                tracing::warn!(thread = %id, error = %format!("{e:#}"), "could not start a compaction before the prompt cache expires");
            }
            // Not started (the thread was not free after all, its start could not be
            // recorded): not left to run at some later dispatch, before a message.
            if let Some(rt) = self.rt.get_mut(&id) {
                if rt.compact.as_ref().is_some_and(|a| a.by == CompactBy::Idle) {
                    rt.compact = None;
                }
            }
        }
    }

    /// The compaction before expiry thread `thread` is due at `now`, if any.
    fn idle_compaction_due(
        &self,
        hub: &Inner,
        thread: &str,
        now: DateTime<Utc>,
    ) -> Option<IdleOffer> {
        let offer = self.idle_offer_basis(hub, thread)?;
        let view = self.registry.get(thread)?;
        if view.retired
            || view
                .cache_ttl_s
                .is_none_or(|t| t < IDLE_COMPACTION_MIN_TTL_S)
            || view.current_turn.is_some()
            || view.is_blocked()
            || view.stopped
            || view.crashed
            || !self.may_take_turn(thread)
        {
            return None;
        }
        let lead = chrono::Duration::from_std(hub.idle_compaction_lead).ok()?;
        let margin = chrono::Duration::from_std(hub.idle_compaction_margin).ok()?;
        if now < offer.expires_at - lead || now >= offer.expires_at - margin {
            return None;
        }
        if self.store.pending_count(thread).ok()? > 0 {
            return None;
        }
        // One per cache: its latest turn a compaction (done, or failed) is enough.
        if view.turns > 0 && self.is_compaction(thread, view.turns).unwrap_or(true) {
            return None;
        }
        Some(offer)
    }
}

/// The resolution closing offer card `attn_id` without an answer.
fn offer_closed(attn_id: &str, by: ResolvedBy) -> EventBody {
    EventBody::AttentionResolved(AttentionResolved {
        attn_id: attn_id.to_string(),
        by,
        answer: None,
        ..Default::default()
    })
}

/// The `conversation.update` that turns thread `name`'s offers off (`off`) or on.
fn offers_update(name: &str, off: bool) -> EventBody {
    let text = if off {
        format!(
            "No more offers to compact {name}'s context when its prompt cache has expired \
             (`twr compact {name} --offers on` turns them back on)."
        )
    } else {
        format!("Offers to compact {name}'s context when its prompt cache has expired are on.")
    };
    EventBody::ConversationUpdate(ConversationUpdate {
        text,
        ext: twr_ext_map(json!({ IDLE_COMPACTION_KEY: if off { "off" } else { "on" } })),
        ..Default::default()
    })
}

/// `d` in hours and minutes (`1h 12m`, `12m`), seconds under a minute.
fn span(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    match (s / 3600, (s % 3600) / 60) {
        (0, 0) => format!("{s}s"),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// Compacts idle workers before their prompt cache expires (`before_expiry`): every `period`,
/// the first one `period` after it starts (`Hub::open` starts it once its recovery is done),
/// until the hub stops or is dropped.
pub(super) async fn idle_compaction_ticker(hub: Weak<Inner>, period: Duration) {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(inner) = hub.upgrade() else {
            return;
        };
        if *inner.state.borrow() != HubState::Running {
            return;
        }
        inner.core().compact_before_expiry(&inner, Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_as_hours_and_minutes() {
        let s = chrono::Duration::seconds;
        assert_eq!(span(s(45)), "45s");
        assert_eq!(span(s(-3)), "0s");
        assert_eq!(span(s(12 * 60 + 5)), "12m");
        assert_eq!(span(s(3600)), "1h");
        assert_eq!(span(s(3600 + 12 * 60)), "1h 12m");
    }

    /// Every answer of the user's (`Send as is`, `Don't ask`, a dismissal) lets a queue a
    /// failure held go, as their send does: the card closed, nothing else would. (A hold that
    /// comes while a card is open, set here directly: no runtime path the tests drive makes
    /// one.)
    #[tokio::test]
    async fn every_answer_lets_a_held_queue_go() {
        use std::sync::Arc;
        use twr_core::adapter::{Adapter, Answer};
        use twr_core::config::Config;

        use super::super::{Hub, HubOptions, NewThread};

        let dir = tempfile::tempdir().unwrap();
        let mut opts = HubOptions::new(dir.path().join("twr.db"), Config::default());
        opts.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
            (name == "fake").then(|| {
                Arc::new(twr_adapters::FakeAdapter::new().with_compaction()) as Arc<dyn Adapter>
            })
        });
        let hub = Hub::open(opts).await.unwrap();
        let user = Caller::user("c");
        let wait = |what: &'static str, pred: Box<dyn Fn(&Core) -> bool>| {
            let inner = Arc::clone(&hub.inner);
            async move {
                for _ in 0..1000 {
                    if pred(&inner.core()) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("timed out waiting for {what}");
            }
        };
        for (name, answer) in [
            ("as-is", Some(SEND_AS_IS)),
            ("dont", Some("Don't ask for dont")),
            ("dismissed", None),
        ] {
            let created = hub
                .new_thread(
                    NewThread {
                        project: dir.path().to_string_lossy().into_owned(),
                        runtime: Some("fake".into()),
                        name: Some(name.into()),
                        ..Default::default()
                    },
                    "c",
                )
                .await
                .unwrap();
            let t = created["id"].as_str().unwrap().to_string();
            hub.send_by(&t, "!context 250000\n!cache 3600 3700\nwarm up", &user)
                .unwrap();
            let id = t.clone();
            wait(
                "the first turn",
                Box::new(move |c| {
                    c.view(&id)
                        .is_ok_and(|v| v.turns == 1 && v.current_turn.is_none())
                }),
            )
            .await;
            hub.send_by(&t, "next", &user).unwrap();
            let id = t.clone();
            wait(
                "a card",
                Box::new(move |c| c.idle_offer_card(&id).is_some()),
            )
            .await;
            let attn_id = {
                let mut core = hub.inner.core();
                core.rt(&t).queue_held = true;
                core.idle_offer_card(&t).unwrap().0
            };
            match answer {
                Some(a) => hub.respond_by(&attn_id, Answer::new(a), &user).map(drop),
                None => hub.attention_dismiss(&attn_id, None, &user).map(drop),
            }
            .unwrap();
            let id = t.clone();
            wait(
                "the held message",
                Box::new(move |c| {
                    c.view(&id)
                        .is_ok_and(|v| v.turns == 2 && v.current_turn.is_none())
                }),
            )
            .await;
            assert!(!hub.inner.core().rt.get(&t).unwrap().queue_held, "{name}");
        }
        hub.shutdown().await;
    }
}
