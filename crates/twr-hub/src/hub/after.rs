//! Prompts that wait for other threads: `thread.send {after}` (`twr send --after`, the MCP
//! `send`'s `after`).
//!
//! A gated prompt is recorded as a `conversation.update` on the thread it is for, with
//! `ext.twr.send_after = {id, prompt, after: [{thread, turn}], source, from}`: `turn` is the last
//! turn each thread it waits for had ended then. It goes out when every one of them has
//! finished a turn after that one and settled: no turn open, no background work or job, no
//! prompt queued, its report written, no wake of the hub's pending for it (`hub::wake`), and
//! nothing from its runtime for [`GRACE`] (a runtime may start a turn of its own once its
//! background work ends). A thread parked or stopped
//! since has settled at once, whatever is queued for it: nothing runs it until it is resumed
//! (a completed turn whose report is missing then counts as not finished well). Then the
//! prompt is queued on the thread as its sender sent it (the user, or the orchestrator),
//! `{report:<thread>}` in it replaced by that thread's report, in one batch with the
//! `conversation.update` that ends it (`ext.twr.send_after_end = {id, outcome: sent}`).
//!
//! When one of them finished otherwise (its turn failed, was interrupted, or its report says it
//! gave up or needs verification), a card on the hub's thread asks the user: `Send` sends it
//! anyway, `Drop` drops it (`outcome: dropped`). The card is advisory: a later finished turn
//! of that thread (an automatic retry) sends it and closes the card. `thread.send_cancel` drops
//! it (`outcome: cancelled`); so does the deletion of its thread or of one it waits for.
//!
//! The pending prompts come back from the log at startup, and each is checked again then.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};

use super::orch::cut;
use super::subscribe::wait_state;
use super::{append_err, rpc, twr_ext, twr_ext_map, Batch, Caller, Core, HubState, Inner};

/// `conversation.update` `ext.twr` key: a gated prompt.
pub(super) const SEND_AFTER_KEY: &str = "send_after";
/// `conversation.update` `ext.twr` key: a gated prompt's end, `{id, outcome, why?}`.
pub(super) const SEND_AFTER_END_KEY: &str = "send_after_end";
/// `attention.raised` `ext.twr` key of a card about a gated prompt: `{id}`.
pub(super) const AFTER_CARD_KEY: &str = "after_card";
/// The card's option that sends the prompt anyway.
pub const SEND: &str = "Send";
/// The card's option that drops it.
pub const DROP: &str = "Drop";
/// Most threads one prompt waits for.
pub const AFTER_MAX: usize = 8;
/// Most gated prompts at once.
pub const PENDING_MAX: usize = 64;
/// How long a thread a prompt waits for must have settled before it counts as finished.
pub const GRACE: Duration = Duration::from_secs(5);
/// Longest report a `{report:<thread>}` puts in, in bytes.
pub const REPORT_MAX: usize = 8000;
/// Longest prompt reports are put into, in bytes: past it, a placeholder says it was left out.
pub const FILLED_MAX: usize = 256 << 10;

/// One thread a prompt waits for: its id, and the last turn it had ended when the prompt was
/// recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Gate {
    pub(super) thread: String,
    pub(super) turn: u64,
}

/// A gated prompt (`ext.twr.send_after`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Gated {
    pub(super) id: String,
    pub(super) prompt: String,
    pub(super) after: Vec<Gate>,
    pub(super) source: Source,
    pub(super) from: TurnFrom,
    /// The thread it is for (the event's).
    #[serde(skip)]
    pub(super) target: String,
    /// Its open card, if any.
    #[serde(skip)]
    pub(super) card: Option<String>,
}

impl Gated {
    /// Whether it is for thread `thread` or waits for it.
    fn involves(&self, thread: &str) -> bool {
        self.target == thread || self.after.iter().any(|a| a.thread == thread)
    }
}

/// The gated prompts, from the log; and the channel to [`after_task`]. In the hub's
/// checkpoints (`hub::checkpoint`) as [`AfterFold`].
#[derive(Default)]
pub(super) struct AfterState {
    pub(super) pending: BTreeMap<String, Gated>,
    /// Open cards: attention id → gated prompt id.
    pub(super) cards: HashMap<String, String>,
    /// Gated prompts a deletion dropped (of their thread or of one they wait for), with why,
    /// whose end is not recorded yet: [`after_task`] records it, and closes their card. A
    /// deletion is final for them, whatever is restored later.
    orphans: BTreeMap<String, (Gated, String)>,
    tx: Option<mpsc::UnboundedSender<String>>,
}

/// [`AfterState`] as a checkpoint has it: all but the channel, each gated prompt with what its
/// own serialization (`ext.twr.send_after`) leaves out.
#[derive(Serialize, Deserialize)]
struct AfterFold {
    pending: BTreeMap<String, GatedFold>,
    cards: HashMap<String, String>,
    orphans: BTreeMap<String, (GatedFold, String)>,
}

/// A [`Gated`] whole.
#[derive(Serialize, Deserialize)]
struct GatedFold {
    gated: Gated,
    target: String,
    card: Option<String>,
}

impl GatedFold {
    fn of(g: &Gated) -> Self {
        GatedFold {
            gated: g.clone(),
            target: g.target.clone(),
            card: g.card.clone(),
        }
    }

    fn into_gated(self) -> Gated {
        Gated {
            target: self.target,
            card: self.card,
            ..self.gated
        }
    }
}

/// Equal when they hold the same prompts and cards, and both have the channel or neither.
impl PartialEq for AfterState {
    fn eq(&self, other: &Self) -> bool {
        self.pending == other.pending
            && self.cards == other.cards
            && self.orphans == other.orphans
            && self.tx.is_some() == other.tx.is_some()
    }
}

impl Serialize for AfterState {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        AfterFold {
            pending: self
                .pending
                .iter()
                .map(|(id, g)| (id.clone(), GatedFold::of(g)))
                .collect(),
            cards: self.cards.clone(),
            orphans: self
                .orphans
                .iter()
                .map(|(id, (g, why))| (id.clone(), (GatedFold::of(g), why.clone())))
                .collect(),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for AfterState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let f = AfterFold::deserialize(d)?;
        Ok(AfterState {
            pending: f
                .pending
                .into_iter()
                .map(|(id, g)| (id, g.into_gated()))
                .collect(),
            cards: f.cards,
            orphans: f
                .orphans
                .into_iter()
                .map(|(id, (g, why))| (id, (g.into_gated(), why)))
                .collect(),
            tx: None,
        })
    }
}

/// A wake of [`after_task`] for no thread in particular: the orphans.
const ORPHANS: &str = "";

/// How often every gated prompt is checked again, whatever happened.
const SWEEP: Duration = Duration::from_secs(30);

/// How long after a failure to record what a gated prompt came to it is tried again.
const RETRY: Duration = Duration::from_secs(5);

impl AfterState {
    /// Whether it has the channel to [`after_task`], which checkpoints skip (tests).
    #[cfg(test)]
    pub(super) fn has_channel(&self) -> bool {
        self.tx.is_some()
    }

    /// Whether thread `thread` is one a gated prompt is for or waits for.
    fn concerns(&self, thread: &str) -> bool {
        self.pending.values().any(|g| g.involves(thread))
    }
}

/// How a thread a prompt waits for stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateState {
    /// Not finished since (or still settling: check again at the instant given).
    Waiting(Option<Instant>),
    /// Finished a turn, completed and reported fine.
    Done,
    /// Finished otherwise: failed, interrupted, gave up or needs verification.
    NotOk,
}

impl Core {
    /// Folds `ev` (live and on replay): the gated prompts and their cards.
    pub(super) fn track_after(&mut self, ev: &Event) {
        match &ev.body {
            EventBody::ConversationUpdate(p) => {
                let Some(twr) = twr_ext(&p.ext) else {
                    return;
                };
                if let Some(v) = twr.get(SEND_AFTER_KEY) {
                    if let Ok(mut g) = serde_json::from_value::<Gated>(v.clone()) {
                        g.target = ev.thread.clone();
                        self.after.pending.insert(g.id.clone(), g);
                    }
                }
                if let Some(id) = twr
                    .get(SEND_AFTER_END_KEY)
                    .and_then(|e| e.get("id"))
                    .and_then(Value::as_str)
                {
                    self.after.pending.remove(id);
                    self.after.orphans.remove(id);
                }
            }
            // Final for the prompts it concerns: ended by [`after_task`].
            EventBody::ThreadDeleted(_) => {
                let gone: Vec<String> = self
                    .after
                    .pending
                    .values()
                    .filter(|g| g.involves(&ev.thread))
                    .map(|g| g.id.clone())
                    .collect();
                for id in gone {
                    if let Some(g) = self.after.pending.remove(&id) {
                        let name = self.thread_name(&ev.thread);
                        let why = if g.target == ev.thread {
                            "its thread was deleted".to_string()
                        } else {
                            format!("{name}, which it waited for, was deleted")
                        };
                        self.after.orphans.insert(id, (g, why));
                    }
                }
            }
            EventBody::AttentionRaised(p)
                if ev.thread == HUB_THREAD && ev.source == Source::Hub =>
            {
                if let Some(id) = twr_ext(&p.ext)
                    .and_then(|t| t.get(AFTER_CARD_KEY))
                    .and_then(|c| c.get("id"))
                    .and_then(Value::as_str)
                {
                    if let Some(g) = self.after.pending.get_mut(id) {
                        g.card = Some(p.attn_id.clone());
                        self.after.cards.insert(p.attn_id.clone(), id.to_string());
                    }
                }
            }
            EventBody::AttentionResolved(p) => {
                if let Some(id) = self.after.cards.remove(&p.attn_id) {
                    if let Some(g) = self.after.pending.get_mut(&id) {
                        g.card = None;
                    }
                    if let Some((g, _)) = self.after.orphans.get_mut(&id) {
                        g.card = None;
                    }
                }
            }
            _ => {}
        }
    }

    /// Live event `ev` of a thread a gated prompt concerns: [`after_task`] checks them again
    /// (and ends what a deletion dropped).
    pub(super) fn feed_after(&self, ev: &Event) {
        if let Some(tx) = &self.after.tx {
            if self.after.concerns(&ev.thread) {
                let _ = tx.send(ev.thread.clone());
            }
            if !self.after.orphans.is_empty() {
                let _ = tx.send(ORPHANS.to_string());
            }
        }
    }

    /// `thread.send {after}`: records `prompt` for thread `id`, to go out once each of `after`
    /// has finished its next turn (see the module docs).
    pub(super) fn send_after(
        &mut self,
        hub: &Inner,
        id: &str,
        prompt: &str,
        after: &[String],
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if prompt.trim().is_empty() {
            return Err(rpc(codes::BAD_REQUEST, "prompt is empty"));
        }
        super::compact::refuse_compact_command(prompt, caller.from())?;
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let view = self.view(id)?.clone();
        self.refuse_retired(id)?;
        self.refuse_deleting(id)?;
        self.refuse_switching(id)?;
        // Never for the orchestrator: a message to it is the user's word at the moment it was
        // written (what it may answer for the user, `hub::orch`), and a report put in it
        // would read as the user's. It watches a thread itself instead (`watch_thread`).
        if view.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "a prompt for the orchestrator can't wait for other threads: tell it now, and \
                 ask it to watch them (it is woken when they finish)",
            ));
        }
        if after.is_empty() || after.len() > AFTER_MAX {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("after: name 1 to {AFTER_MAX} threads"),
            ));
        }
        if self.after.pending.len() >= PENDING_MAX {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{PENDING_MAX} prompts already wait for other threads: cancel one first \
                     (`twr send --cancel <id>`)"
                ),
            ));
        }
        let mut gates: Vec<Gate> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for q in after {
            let g = self.resolve(q)?;
            let gv = self.view(&g)?;
            if g == id {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("{} cannot wait for itself", view.name),
                ));
            }
            if gv.is_orchestrator() {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    "a prompt waits for worker threads, not the orchestrator",
                ));
            }
            if gates.iter().any(|x| x.thread == g) {
                continue;
            }
            names.push(gv.name.clone());
            gates.push(Gate {
                turn: gv.last_turn.as_ref().map_or(0, |t| t.turn),
                thread: g,
            });
        }
        // Never in a circle: a thread it waits for must not itself wait, through held prompts,
        // for this one (each would wait for the other for ever).
        for gate in &gates {
            if self.waits_for(&gate.thread, id) {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "{} already waits for {}, through prompts held for it: they would wait \
                         for each other",
                        self.thread_name(&gate.thread),
                        view.name
                    ),
                ));
            }
        }
        // `{report:<thread>}` names one of them, by any of its names: kept by its id.
        let prompt = self.report_placeholders(prompt, &gates)?;
        let gated = Gated {
            id: new_id(),
            prompt,
            after: gates,
            source: caller.source(),
            from: caller.from(),
            target: id.to_string(),
            card: None,
        };
        let text = format!(
            "Queued until {} finish{}: {}",
            names.join(", "),
            if names.len() == 1 { "es" } else { "" },
            cut(&gated.prompt, 300)
        );
        let body = EventBody::ConversationUpdate(ConversationUpdate {
            text,
            about_thread: None,
            ext: twr_ext_map(json!({ SEND_AFTER_KEY: gated })),
            ..Default::default()
        });
        self.append(hub, id, None, caller.source(), body)
            .map_err(append_err)?;
        if let Some(tx) = &self.after.tx {
            let _ = tx.send(id.to_string());
        }
        Ok(json!({
            "thread": id,
            "thread_name": view.name,
            "queued_after": gated.id,
            "after": names,
        }))
    }

    /// Whether thread `from` waits for thread `to` through held prompts: a prompt held for
    /// `from` waits for `to`, or for a thread that does.
    fn waits_for(&self, from: &str, to: &str) -> bool {
        let mut seen: Vec<&str> = Vec::new();
        let mut next: Vec<&str> = vec![from];
        while let Some(t) = next.pop() {
            if t == to {
                return true;
            }
            if seen.contains(&t) {
                continue;
            }
            seen.push(t);
            for g in self.after.pending.values().filter(|g| g.target == t) {
                next.extend(g.after.iter().map(|a| a.thread.as_str()));
            }
        }
        false
    }

    /// `prompt` with each `{report:<name>}` naming one of `gates` by its id; an error for one
    /// that names another thread.
    fn report_placeholders(&self, prompt: &str, gates: &[Gate]) -> Result<String, RpcError> {
        let mut out = String::with_capacity(prompt.len());
        let mut rest = prompt;
        while let Some(at) = rest.find("{report:") {
            out.push_str(&rest[..at]);
            let after = &rest[at + "{report:".len()..];
            let Some(end) = after.find('}') else {
                out.push_str(&rest[at..]);
                rest = "";
                break;
            };
            let name = &after[..end];
            let id = self
                .resolve(name)
                .ok()
                .filter(|g| gates.iter().any(|x| x.thread == *g));
            let Some(id) = id else {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("{{report:{name}}} names no thread the prompt waits for"),
                ));
            };
            out.push_str(&format!("{{report:{id}}}"));
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// How gate `gate` stands at `now` ([`GateState`]).
    fn gate_state(&self, gate: &Gate, now: Instant) -> GateState {
        let Some(v) = self.registry.get(&gate.thread) else {
            return GateState::Waiting(None);
        };
        let Some(last) = v.last_turn.as_ref().filter(|t| t.turn > gate.turn) else {
            return GateState::Waiting(None);
        };
        // Attached (or being attached): the user works in it; its detach is an event of it.
        // Being deleted: its deletion drops the prompt, or fails and leaves it as it was. On a
        // box not connected and reconciled: what its runtime did meanwhile is not here yet.
        let rt = self.rt.get(&gate.thread);
        let attaching = rt.is_some_and(|r| r.attach.is_some());
        let deleting = rt.is_some_and(|r| r.deleting);
        // Its runtime ending, its end not recorded yet (a crash being reaped, lost output):
        // that end is an event of it.
        let ending = rt.is_some_and(|r| r.exiting || r.giving_up());
        if v.attached.is_some() || attaching || deleting || ending || !self.box_online(&v.box_name)
        {
            return GateState::Waiting(None);
        }
        // Parked or stopped, with no runtime of it running or being resumed: it doesn't go
        // on, whatever is queued for it (prompts wait for its resume, and prompts held for it
        // for other threads would only join them), as for `twr wait`. How the turn it
        // finished ended says how it finished, at once: no runtime of it starts a turn of its
        // own.
        let halted = (v.parked || v.stopped) && !rt.is_some_and(|r| r.live.is_some() || r.resuming);
        let completed = last.result == TurnResult::Completed;
        // Background work the turn left running, ended unfinished by a crash, stop, park,
        // attach or restart since, is no good finish (whatever came after, short of a turn).
        let cut_short = v.crashed || v.cut_short == Some(last.turn);
        let fine = completed
            && !cut_short
            && v.last_report
                .as_ref()
                .is_none_or(|r| !r.grade.needs_review());
        if halted {
            // A completed turn whose report is not written yet counts as not finished well:
            // the card is advisory, and the report, once written, sends the prompt.
            return if fine && v.reported {
                GateState::Done
            } else {
                GateState::NotOk
            };
        }
        // Work queued for it: a prompt, or one that itself waits for other threads.
        let queued = self.store.pending_count(&gate.thread).unwrap_or(0) > 0
            || self.after.pending.values().any(|g| g.target == gate.thread);
        if v.has_work() || queued || v.is_blocked() {
            return GateState::Waiting(None);
        }
        // A completed turn's report is written after it ends.
        if completed && !v.reported {
            return GateState::Waiting(None);
        }
        // The hub is about to wake it after its background work (`hub::wake`): not finished.
        if let Some(due) = self.wake.due(&gate.thread) {
            let left = (due - chrono::Utc::now()).to_std().unwrap_or_default();
            return GateState::Waiting(Some(now + left + Duration::from_secs(1)));
        }
        // Settled long enough: the runtime starts no turn of its own any more.
        let quiet = v
            .last_activity
            .map(|t| (chrono::Utc::now() - t).to_std().unwrap_or_default())
            .unwrap_or(GRACE);
        if quiet < GRACE {
            return GateState::Waiting(Some(now + (GRACE - quiet)));
        }
        if fine {
            GateState::Done
        } else {
            GateState::NotOk
        }
    }

    /// Checks gated prompt `id` again: sends it when every thread it waits for finished well,
    /// or raises its card when one of them finished otherwise (the others may still run).
    /// Returns when to check it again: a thread still settling, or what could not be recorded.
    pub(super) fn check_gated(&mut self, hub: &Arc<Inner>, id: &str) -> Option<Instant> {
        // A hub stopping does nothing with them: the next one checks them again.
        if self.shutting_down {
            return None;
        }
        let g = self.after.pending.get(id)?.clone();
        let now = Instant::now();
        let mut again: Option<Instant> = None;
        let mut waiting = false;
        let mut not_ok: Vec<String> = Vec::new();
        for a in &g.after {
            match self.gate_state(a, now) {
                GateState::Done => {}
                GateState::NotOk => not_ok.push(self.thread_name(&a.thread)),
                GateState::Waiting(at) => {
                    waiting = true;
                    if let Some(at) = at {
                        again = Some(again.map_or(at, |x| x.min(at)));
                    }
                }
            }
        }
        let recorded = if !not_ok.is_empty() {
            if g.card.is_none() {
                self.raise_after_card(hub, &g, &not_ok)
            } else {
                Ok(())
            }
        } else if !waiting {
            self.commit_gated(hub, &g, None, Decision::Send).map(|_| ())
        } else {
            Ok(())
        };
        if let Err(e) = recorded {
            tracing::warn!(id = %g.id, error = %e.message, "cannot record what a prompt queued after other threads came to; trying again");
            let retry = now + RETRY;
            again = Some(again.map_or(retry, |x| x.min(retry)));
        }
        again
    }

    /// Records gated prompt `g`'s end in one batch, with `resolution` (its card's answer)
    /// first: sent (the prompt queued as its sender sent it), or not (`Drop`, `Cancel`), its
    /// card closed. A prompt the thread can't take now is dropped instead, saying why. The
    /// prompt is dispatched once that batch committed.
    fn commit_gated(
        &mut self,
        hub: &Arc<Inner>,
        g: &Gated,
        resolution: Option<(String, EventBody, Source)>,
        decision: Decision,
    ) -> Result<Vec<Event>, RpcError> {
        let names: Vec<String> = g
            .after
            .iter()
            .map(|a| self.thread_name(&a.thread))
            .collect();
        // The user's Send on the card, for a prompt the user queued, is the user sending it
        // ([`Batch::user_send`]); a prompt let go when the threads it waited for finished is not.
        let users_send =
            g.from == TurnFrom::User && matches!(resolution, Some((_, _, Source::User(_))));
        let mut batch = Batch::default();
        let card = resolution
            .as_ref()
            .map(|(a, _, _)| a.clone())
            .or(g.card.clone());
        match resolution {
            Some((_, body, source)) => {
                batch.push(HUB_THREAD, None, source, body);
            }
            None => {
                if let Some(card) = &card {
                    batch.push(HUB_THREAD, None, Source::Hub, card_closed(card));
                }
            }
        }
        let mut outcome = match decision {
            Decision::Send => "sent",
            Decision::Drop => "dropped",
            Decision::Cancel => "cancelled",
        };
        let mut why: Option<String> = None;
        if decision == Decision::Send {
            let prompt = self.filled(g);
            match self.push_prompt(
                &mut batch,
                &g.target,
                &prompt,
                g.from,
                g.source.clone(),
                None,
            ) {
                Ok(()) if users_send => batch.user_send(),
                Ok(()) => {}
                // For now (the hub stopping, the thread being deleted or switched): it stays
                // held, and is tried again.
                Err(e) if e.code == codes::CONFLICT => return Err(e),
                Err(e) => {
                    outcome = "dropped";
                    why = Some(e.message);
                }
            }
        }
        let text = match (outcome, &why) {
            ("sent", _) => format!(
                "Sent the prompt queued until {} finished.",
                names.join(", ")
            ),
            ("cancelled", _) => "Cancelled the prompt queued after other threads.".to_string(),
            (_, Some(why)) => format!("Dropped the prompt queued after other threads: {why}."),
            _ => "Dropped the prompt queued after other threads.".to_string(),
        };
        batch.push(
            &g.target,
            None,
            Source::Hub,
            EventBody::ConversationUpdate(ConversationUpdate {
                text,
                ext: twr_ext_map(
                    json!({ SEND_AFTER_END_KEY: {"id": g.id, "outcome": outcome, "why": why} }),
                ),
                ..Default::default()
            }),
        );
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        if let Some(queued) = done.queued.first().copied() {
            self.dispatch_queued(hub, &g.target, queued);
        }
        Ok(done.events)
    }

    /// Records the end of the gated prompts a deletion dropped, and closes their cards.
    fn end_orphans(&mut self, hub: &Inner) -> Result<(), RpcError> {
        let orphans: Vec<(Gated, String)> = self.after.orphans.values().cloned().collect();
        for (g, why) in orphans {
            let mut batch = Batch::default();
            if let Some(card) = &g.card {
                batch.push(HUB_THREAD, None, Source::Hub, card_closed(card));
            }
            // On its thread, unless that is what was deleted.
            let on = if self.registry.get(&g.target).is_some() {
                g.target.clone()
            } else {
                HUB_THREAD.to_string()
            };
            batch.push(
                &on,
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: format!("Dropped the prompt queued after other threads: {why}."),
                    ext: twr_ext_map(
                        json!({ SEND_AFTER_END_KEY: {"id": g.id, "outcome": "dropped", "why": why} }),
                    ),
                    ..Default::default()
                }),
            );
            self.append_batch(hub, batch).map_err(append_err)?;
        }
        Ok(())
    }

    /// `prompt` of `g` with each `{report:<id>}` replaced by that thread's last report, in one
    /// pass over the prompt as written: a report is put in as it is (a placeholder in it stays
    /// text), and reports stop going in once the prompt would pass [`FILLED_MAX`].
    fn filled(&self, g: &Gated) -> String {
        let mut out = String::with_capacity(g.prompt.len());
        let mut rest = g.prompt.as_str();
        while let Some(at) = rest.find("{report:") {
            out.push_str(&rest[..at]);
            let after = &rest[at + "{report:".len()..];
            let gate = after.find('}').and_then(|end| {
                let id = &after[..end];
                g.after.iter().any(|a| a.thread == id).then_some((id, end))
            });
            let Some((id, end)) = gate else {
                out.push_str("{report:");
                rest = after;
                continue;
            };
            // The report of its latest ended turn, if that one has any (a failed or
            // interrupted one does not: an earlier report is not about it).
            let report = self
                .registry
                .get(id)
                .and_then(|v| {
                    let last = v.last_turn.as_ref()?.turn;
                    v.last_report.as_ref().filter(|r| r.turn == Some(last))
                })
                .map_or_else(
                    || "(no report)".to_string(),
                    |r| cut(&r.summary, REPORT_MAX),
                );
            if out.len() + report.len() + rest.len() > FILLED_MAX {
                out.push_str("(report left out: the prompt would be too long)");
            } else {
                out.push_str(&report);
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// Raises the card asking whether to send `g` though `not_ok` did not finish well.
    fn raise_after_card(
        &mut self,
        hub: &Inner,
        g: &Gated,
        not_ok: &[String],
    ) -> Result<(), RpcError> {
        let target = self.thread_name(&g.target);
        let summary = format!(
            "{} did not finish well (failed, interrupted, gave up or needs verification). Send \
             {target} the prompt queued after {} anyway? {}",
            not_ok.join(", "),
            if g.after.len() == 1 { "it" } else { "them" },
            cut(&g.prompt, 300)
        );
        let body = EventBody::AttentionRaised(AttentionRaised {
            attn_id: new_id(),
            reason: AttentionReason::Question,
            summary,
            options: vec![SEND.into(), DROP.into()],
            ext: twr_ext_map(json!({ AFTER_CARD_KEY: {"id": g.id, "thread": g.target} })),
            ..Default::default()
        });
        self.append(hub, HUB_THREAD, None, Source::Hub, body)
            .map(|_| ())
            .map_err(append_err)
    }

    /// Answers the card `attn_id` of gated prompt `id` (the user only): `Send` sends it now,
    /// anything else drops it; the answer and that in one batch.
    pub(super) fn answer_after_card(
        &mut self,
        hub: &Arc<Inner>,
        attn_id: &str,
        id: &str,
        decision: Option<&str>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{attn_id} asks the user whether to send a prompt queued after other \
                     threads: only the user answers it; tell the user it is waiting for them"
                ),
            ));
        }
        let body = EventBody::AttentionResolved(AttentionResolved {
            attn_id: attn_id.to_string(),
            by: ResolvedBy::User,
            answer: decision.map(twr_core::adapter::Answer::new),
            ..Default::default()
        });
        let Some(g) = self.after.pending.get(id).cloned() else {
            // Its prompt is gone already: the answer only closes it.
            return self
                .append(hub, HUB_THREAD, None, caller.source(), body)
                .map_err(append_err);
        };
        // Send or Drop (as typed, in any case); a dismissal drops it too. Anything else
        // leaves the card and the prompt as they are.
        let send = match decision.map(|d| d.trim().to_lowercase()).as_deref() {
            Some("send") => Decision::Send,
            Some("drop") | None => Decision::Drop,
            Some(other) => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("{other:?} is not an answer to {attn_id}: answer {SEND} or {DROP}"),
                ))
            }
        };
        let resolution = (attn_id.to_string(), body, caller.source());
        self.commit_gated(hub, &g, Some(resolution), send)?
            .into_iter()
            .find(|e| matches!(&e.body, EventBody::AttentionResolved(r) if r.attn_id == attn_id))
            .ok_or_else(|| super::internal("the answer was not recorded"))
    }

    /// `thread.send_cancel {id}`: drops gated prompt `id` (a unique prefix of its id will do),
    /// once its end is recorded.
    pub(super) fn cancel_gated(
        &mut self,
        hub: &Arc<Inner>,
        query: &str,
    ) -> Result<Value, RpcError> {
        let hits: Vec<&Gated> = self
            .after
            .pending
            .values()
            .filter(|g| g.id == query || (query.len() >= 4 && g.id.starts_with(query)))
            .collect();
        let g = match hits.as_slice() {
            [g] => (*g).clone(),
            [] => {
                return Err(rpc(
                    codes::NOT_FOUND,
                    format!("no prompt queued after other threads is {query:?}"),
                ))
            }
            _ => return Err(rpc(codes::AMBIGUOUS, format!("{query:?} names several"))),
        };
        self.commit_gated(hub, &g, None, Decision::Cancel)?;
        Ok(
            json!({"cancelled": g.id, "thread": g.target, "thread_name": self.thread_name(&g.target)}),
        )
    }

    /// For deleting threads `ids`: the held prompts each one's deletion drops, each counted
    /// once: under its own thread when that is deleted too, else under the first of `ids` it
    /// waits for.
    pub(super) fn held_lost(&self, ids: &[&str]) -> HashMap<String, u64> {
        let mut lost: HashMap<String, u64> = HashMap::new();
        for g in self.after.pending.values() {
            let on = if ids.contains(&g.target.as_str()) {
                Some(g.target.as_str())
            } else {
                ids.iter()
                    .copied()
                    .find(|i| g.after.iter().any(|a| a.thread == *i))
            };
            if let Some(on) = on {
                *lost.entry(on.to_string()).or_default() += 1;
            }
        }
        lost
    }

    /// Stages, in `batch`, the end of the prompts held for thread `id` (it is being stopped:
    /// its queue goes), and the closing of their cards. Returns how many.
    pub(super) fn stage_held_drops(&self, batch: &mut Batch, id: &str, why: &str) -> usize {
        let mut n = 0;
        for g in self.after.pending.values().filter(|g| g.target == id) {
            if let Some(card) = &g.card {
                batch.push(HUB_THREAD, None, Source::Hub, card_closed(card));
            }
            batch.push(
                id,
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: format!("Dropped the prompt queued after other threads: {why}."),
                    ext: twr_ext_map(
                        json!({ SEND_AFTER_END_KEY: {"id": g.id, "outcome": "dropped", "why": why} }),
                    ),
                    ..Default::default()
                }),
            );
            n += 1;
        }
        n
    }

    /// The names of the threads the gated prompts for thread `id` wait for, each once.
    pub(super) fn gated_names(&self, id: &str) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for g in self.after.pending.values().filter(|g| g.target == id) {
            for a in &g.after {
                let name = self.thread_name(&a.thread);
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    /// The gated prompts for thread `id` (`thread.get`'s `queued_after`).
    pub(super) fn gated_json(&self, id: &str) -> Vec<Value> {
        self.after
            .pending
            .values()
            .filter(|g| g.target == id)
            .map(|g| {
                json!({
                    "id": g.id,
                    "after": g.after.iter().map(|a| self.thread_name(&a.thread)).collect::<Vec<_>>(),
                    "prompt": cut(&g.prompt, 300),
                    "card": g.card,
                })
            })
            .collect()
    }
}

/// What became of a gated prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Send,
    Drop,
    Cancel,
}

/// The resolution closing card `attn_id` without an answer (its prompt went or was dropped).
fn card_closed(attn_id: &str) -> EventBody {
    EventBody::AttentionResolved(AttentionResolved {
        attn_id: attn_id.to_string(),
        by: ResolvedBy::Timeout,
        answer: None,
        ..Default::default()
    })
}

/// Starts [`after_task`] for `inner`: its channel goes into the core, and every gated prompt
/// is checked once.
pub(super) fn start(inner: &Arc<Inner>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let ids: Vec<String> = {
        let mut core = inner.core();
        core.after.tx = Some(tx.clone());
        core.after
            .pending
            .values()
            .map(|g| g.target.clone())
            .collect()
    };
    for id in ids {
        let _ = tx.send(id);
    }
    // What deletions dropped before this start, unrecorded.
    let _ = tx.send(ORPHANS.to_string());
    tokio::spawn(after_task(
        Arc::downgrade(inner),
        rx,
        inner.state.subscribe(),
    ));
}

/// Checks the gated prompts that concern each thread it hears of, until the hub stops: at
/// once, and again when a thread they wait for has settled for [`GRACE`].
async fn after_task(
    hub: Weak<Inner>,
    mut rx: mpsc::UnboundedReceiver<String>,
    mut state: watch::Receiver<HubState>,
) {
    // When to check the gated prompts again, by the thread whose events said so.
    let mut due: HashMap<String, Instant> = HashMap::new();
    // And all of them now and then: a change that is no event of their threads (a box
    // reconnected, a deletion that failed) is seen then.
    let mut sweep = tokio::time::interval(SWEEP);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let next = due.values().min().copied();
        tokio::select! {
            biased;
            _ = wait_state(&mut state, |s| *s != HubState::Running) => return,
            _ = sweep.tick() => {
                let Some(hub) = hub.upgrade() else {
                    return;
                };
                let now = Instant::now();
                for g in hub.core().after.pending.values() {
                    due.entry(g.target.clone()).or_insert(now);
                }
            }
            t = rx.recv() => match t {
                Some(t) => {
                    due.insert(t, Instant::now());
                }
                None => return,
            },
            _ = sleep_until_opt(next) => {
                let Some(hub) = hub.upgrade() else {
                    return;
                };
                let now = Instant::now();
                let ready: Vec<String> = due
                    .iter()
                    .filter(|(_, at)| **at <= now)
                    .map(|(t, _)| t.clone())
                    .collect();
                let mut core = hub.core();
                for t in ready {
                    due.remove(&t);
                    if t == ORPHANS {
                        if let Err(e) = core.end_orphans(&hub) {
                            tracing::warn!(error = %e.message, "cannot record the end of prompts a deletion dropped; trying again");
                            due.insert(t.clone(), now + RETRY);
                        }
                        continue;
                    }
                    let ids: Vec<String> = core
                        .after
                        .pending
                        .values()
                        .filter(|g| g.target == t || g.after.iter().any(|a| a.thread == t))
                        .map(|g| g.id.clone())
                        .collect();
                    for id in ids {
                        if let Some(at) = core.check_gated(&hub, &id) {
                            let e = due.entry(t.clone()).or_insert(at);
                            *e = (*e).min(at);
                        }
                    }
                }
            }
        }
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}
