//! Confirm cards: what the orchestrator proposed from the user's message,
//! for the user to confirm.
//!
//! When the orchestrator's answer or mode change passes provenance (`hub::orch`) but the user's
//! message is not one short command asking for exactly it (`twr_decisions::code::command`), the
//! hub
//! does not refuse it: it records it as a proposal in the orchestrator's turn and returns
//! `{status: "needs_confirmation"}`. When that turn ends (or its runtime stops), the turn's
//! proposals go on one confirm card ([`Core::raise_card`], from the notifier): a `question` the
//! hub raises on [`HUB_THREAD`], with options [`CONFIRM`] and [`CANCEL`], a summary listing
//! every action ("From your message: allow api (Bash: git fetch ...) · set atlas → auto") and
//! `ext.twr.proposal` holding them ([`Proposal`]). The summary is a lossless rendering of exactly
//! those actions (`twr_decisions::code::card`): thread names, labels, mode ids and item
//! summaries have their invisible characters written as `\u{200B}` escapes, and a mode id the
//! thread's runtime does not know is marked `custom mode "..."`; a summary that would not read
//! back as its actions raises no card. Actions that no longer hold when the card is
//! raised (the item was answered or raised again, the thread is in that mode already) are left
//! off; with none left there is no card. The card is rebuilt from the log on replay
//! (`Core::hub_cards`).
//!
//! Only the user answers a confirm card: a TUI key, `twr respond`, or `attention.respond` (or
//! `attention.dismiss`) from a user client. The orchestrator can neither answer nor annotate it
//! (`relay_forbidden`). Confirm ([`Core::confirm_card`]) applies every action at that moment,
//! each checked again (the item still open at the same revision, its runtime running, the
//! thread a worker in the life it was proposed in (not deleted and restored since), a mode
//! valid and the thread still on the runtime and ACP agent it was proposed for (not moved
//! since, as the orchestrator may do without a card)) and recorded as the user's (`attention.resolved{by: user}`,
//! `thread.mode_changed{by: user}`, with `ext.twr.proposal` = the card's id); actions that no
//! longer hold are skipped. The card's own `attention.resolved` lists what was applied and
//! what was skipped and why (`ext.twr.confirmed`); it and every applied action's events are
//! one batch, so a crash or a write failure leaves all of it or none of it, the card
//! still open. Once it committed, every applied answer is forwarded to its runtime first,
//! whatever the order of the actions; then come the answers' dispatches and the restarts they
//! deferred, then the mode changes' restarts and dispatches, so a mode change never
//! stops a runtime before it got the answers to the items it raised between turns. Answering a resolved card again is `conflict` stating how it ended. Cancel (or
//! a dismissal) applies nothing.
//! The orchestrator gets a notice of the outcome. A newer user message to the orchestrator
//! drops the proposals not on a card yet and resolves an open card as superseded, in the batch
//! that records the message's arrival (`Core::mark_user_prompt`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use twr_core::adapter::Answer;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::AttentionItem;
use twr_decisions::code::card::{self, CardLine};

use super::attention::{answer_of, StagedAnswer};
use super::mode::StagedMode;
use super::orch::cut;
use super::{
    append_err, internal, rpc, runtime_spec, twr_ext, twr_ext_map, AnswerFor, Batch, Caller, Core,
    Inner, Response,
};

/// The option that applies a confirm card.
pub const CONFIRM: &str = "Confirm";

/// The option that drops a confirm card.
pub const CANCEL: &str = "Cancel";

/// The answer recorded on a confirm card a newer user message superseded.
pub const SUPERSEDED: &str = "superseded";

/// `ext.twr` key of a confirmed card's `attention.resolved`: `{applied, skipped}`.
const CONFIRMED_KEY: &str = "confirmed";

/// Longest item summary quoted on a card, in bytes.
const ITEM_SUMMARY_MAX: usize = 60;

/// One proposed action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Proposed {
    /// Answer item `attn_id` (at `revision`) of `thread` with option `answer` (as listed).
    Answer {
        attn_id: String,
        revision: u64,
        thread: String,
        answer: String,
    },
    /// Set `thread` to permission mode `mode` (canonical id), in the life it was proposed for
    /// (`lifecycle`, [`twr_core::reduce::ThreadView::lifecycle_seq`]; 0 when not recorded):
    /// a thread deleted and restored since is not changed. `runtime` and `acp_command` are the
    /// thread's when it was proposed (`None`: not recorded): a mode means something else on
    /// another runtime, and the orchestrator can move a thread in a restricted mode without a
    /// card (`hub::model`), so a thread on another runtime (or ACP agent) since is not changed.
    Mode {
        thread: String,
        mode: String,
        #[serde(default)]
        lifecycle: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acp_command: Option<Value>,
    },
    /// Move `thread` to runtime `runtime` (as the orchestrator named it: an agent runtime or a
    /// configured preset) with `model` and `effort` as given (`None`: the new runtime's
    /// default), in the life it was proposed for (`hub::model`).
    Move {
        thread: String,
        runtime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default)]
        lifecycle: u64,
    },
}

/// The proposals of one orchestrator turn that ended, not on a card yet.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PendingCard {
    /// The orchestrator thread.
    pub(super) orchestrator: String,
    /// The orchestrator turn that carried the user's message.
    pub(super) turn: u64,
    /// That message's `said_after`.
    pub(super) said_after: Option<u64>,
    /// The actions, in the order proposed.
    pub(super) actions: Vec<Proposed>,
}

/// `ext.twr.proposal` of a confirm card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Proposal {
    /// The orchestrator thread.
    pub(super) orchestrator: String,
    /// The orchestrator turn that carried the user's message.
    pub(super) turn: u64,
    /// That message's `said_after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) said_after: Option<u64>,
    /// The actions.
    pub(super) actions: Vec<Proposed>,
}

impl Proposal {
    /// From a confirm card's `attention.raised`.
    pub(super) fn from_raised(p: &AttentionRaised) -> Option<Proposal> {
        serde_json::from_value(twr_ext(&p.ext)?.get(PROPOSAL_KEY)?.clone()).ok()
    }
}

/// What a confirmed card's `attention.resolved` says was applied and skipped.
fn confirmed(p: &AttentionResolved) -> Option<(Vec<String>, Vec<String>)> {
    let c = twr_ext(&p.ext)?.get(CONFIRMED_KEY)?;
    let list = |k: &str| -> Vec<String> {
        c.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    Some((list("applied"), list("skipped")))
}

/// Whether thread `view` runs on `runtime` with ACP agent `acp_command` (a [`Proposed::Mode`]'s:
/// `None` was not recorded, and matches no thread).
fn on_runtime(
    view: &twr_core::reduce::ThreadView,
    runtime: Option<&str>,
    acp_command: Option<&Value>,
) -> bool {
    runtime == Some(view.runtime.as_str()) && acp_command == view.acp_command.as_ref()
}

/// What the actions of a confirm card staged before the one being staged change about their
/// threads ([`Core::confirm_card`]): the mode a thread is set to and the runtime it moves to,
/// by thread id.
#[derive(Debug, Default)]
struct Earlier {
    modes: std::collections::HashMap<String, String>,
    runtimes: std::collections::HashMap<String, String>,
}

/// A confirm card's action staged into its batch ([`Core::confirm_card`]).
enum Staged {
    Answer(StagedAnswer),
    Mode(StagedMode),
    /// A move to another runtime: the thread whose settings apply once committed.
    Move(String),
}

/// What the orchestrator is told about a proposed move to another runtime.
const NEEDS_CONFIRMATION_MOVE: &str = "Not applied yet: moving a thread to another runtime \
    can change what its permission mode lets it do without asking, so the user confirms it. \
    It goes on a confirm card when your turn ends, with everything else you proposed in this \
    turn. Tell the user: \"I've put that on a card for you to confirm.\"";

/// What the orchestrator is told about a proposal.
const NEEDS_CONFIRMATION: &str = "Not applied yet: the user's message is not one short \
    command that asks for exactly this (or it also asks for something else). Do not retry with \
    other wording. It goes on a confirm card when your turn ends, with everything else you proposed in this turn, and only the user can \
    confirm it. Tell the user: \"I've put that on a card for you to confirm.\"";

impl Core {
    /// Records the orchestrator's answer `label` to `item` as a proposal; returns the
    /// `needs_confirmation` result.
    pub(super) fn propose_answer(&mut self, item: &AttentionItem, label: &str) -> Value {
        self.orch.record_proposal(Proposed::Answer {
            attn_id: item.attn_id.clone(),
            revision: item.revision,
            thread: item.thread.clone(),
            answer: label.to_string(),
        });
        json!({
            "status": "needs_confirmation",
            "attn_id": item.attn_id,
            "answer": label,
            "message": NEEDS_CONFIRMATION,
        })
    }

    /// Records the orchestrator's change of `thread` to `mode` as a proposal; returns the
    /// `needs_confirmation` result.
    pub(super) fn propose_mode(&mut self, thread: &str, mode: &str) -> Value {
        let view = self.registry.get(thread);
        self.orch.record_proposal(Proposed::Mode {
            thread: thread.to_string(),
            mode: mode.to_string(),
            lifecycle: view.map_or(0, |v| v.lifecycle_seq),
            runtime: view.map(|v| v.runtime.clone()),
            acp_command: view.and_then(|v| v.acp_command.clone()),
        });
        json!({
            "status": "needs_confirmation",
            "thread": thread,
            "thread_name": self.thread_name(thread),
            "mode": mode,
            "message": NEEDS_CONFIRMATION,
        })
    }

    /// Records the orchestrator's move of `thread` to runtime `runtime` (with `model` and
    /// `effort` as it gave them) as a proposal; returns the `needs_confirmation` result.
    pub(super) fn propose_move(
        &mut self,
        thread: &str,
        runtime: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Value {
        self.orch.record_proposal(Proposed::Move {
            thread: thread.to_string(),
            runtime: runtime.to_string(),
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            lifecycle: self.registry.get(thread).map_or(0, |v| v.lifecycle_seq),
        });
        json!({
            "status": "needs_confirmation",
            "thread": thread,
            "thread_name": self.thread_name(thread),
            "runtime": runtime,
            "message": NEEDS_CONFIRMATION_MOVE,
        })
    }

    /// The name of thread `thread` (its id when it has none in the registry).
    pub(super) fn thread_name(&self, thread: &str) -> String {
        self.registry
            .get(thread)
            .map_or(thread.to_string(), |v| v.name.clone())
    }

    /// `action` as a card shows it ([`card::CardLine`]); with `detail`, an answer quotes its
    /// item's summary (cut) too. A mode id the thread's runtime does not know as one of its
    /// own is a custom mode.
    fn card_line(&self, action: &Proposed, detail: bool) -> CardLine {
        match action {
            Proposed::Answer {
                attn_id,
                thread,
                answer,
                ..
            } => CardLine::Answer {
                thread: self.thread_name(thread),
                answer: answer.clone(),
                item: self
                    .attention
                    .get(attn_id)
                    .filter(|_| detail)
                    .map(|i| cut(&i.summary, ITEM_SUMMARY_MAX)),
            },
            Proposed::Mode { thread, mode, .. } => CardLine::Mode {
                thread: self.thread_name(thread),
                mode: mode.clone(),
                custom: !self
                    .registry
                    .get(thread)
                    .is_some_and(|v| runtime_spec::known_mode(mode, &v.runtime)),
            },
            Proposed::Move {
                thread,
                runtime,
                model,
                effort,
                ..
            } => CardLine::Move {
                thread: self.thread_name(thread),
                runtime: runtime.clone(),
                model: model.clone(),
                effort: effort.clone(),
            },
        }
    }

    /// `action` in a few words, every name, label and id written losslessly
    /// ([`card::line`]): "allow api", `answer web: "Use SQLite"`, "set atlas → auto",
    /// `set gem → custom mode "plan\u{200B}"`.
    fn describe(&self, action: &Proposed) -> String {
        card::line(&self.card_line(action, false))
    }

    /// Whether `action` still means something now: the item is open at the same revision, the
    /// thread exists in the life it was proposed for and is not in that mode already.
    fn still_proposed(&self, action: &Proposed) -> bool {
        match action {
            Proposed::Answer {
                attn_id, revision, ..
            } => self
                .attention
                .get(attn_id)
                .is_some_and(|i| i.revision == *revision),
            Proposed::Mode {
                thread,
                mode,
                lifecycle,
                runtime,
                acp_command,
            } => self.registry.get(thread).is_some_and(|v| {
                !v.retired
                    && v.mode != *mode
                    && v.lifecycle_seq == *lifecycle
                    && on_runtime(v, runtime.as_deref(), acp_command.as_ref())
            }),
            Proposed::Move {
                thread, lifecycle, ..
            } => self
                .registry
                .get(thread)
                .is_some_and(|v| !v.retired && v.lifecycle_seq == *lifecycle),
        }
    }

    /// Raises the confirm card for the proposals of an orchestrator turn that ended
    /// ([`super::orch::OrchState::card`]), without the actions that no longer hold; nothing when
    /// none is left. Called by the notifier ([`Core::maybe_notify`]).
    pub(super) fn raise_card(&mut self, hub: &Inner) -> anyhow::Result<()> {
        if self.shutting_down {
            return Ok(());
        }
        let Some(card) = self.orch.card.take() else {
            return Ok(());
        };
        let actions: Vec<Proposed> = card
            .actions
            .iter()
            .filter(|a| self.still_proposed(a))
            .cloned()
            .collect();
        if actions.is_empty() {
            return Ok(());
        }
        // The summary is a faithful rendering of exactly the actions stored with it:
        // every name, label, id and item summary escaped, before `display_safe` sanitizes it.
        let lines: Vec<CardLine> = actions.iter().map(|a| self.card_line(a, true)).collect();
        let summary = card::render(&lines);
        if card::parse(&summary).as_ref() != Some(&lines) {
            tracing::error!(%summary, "a confirm card's text does not read back as its actions; no card");
            return Ok(());
        }
        let proposal = Proposal {
            orchestrator: card.orchestrator.clone(),
            turn: card.turn,
            said_after: card.said_after,
            actions,
        };
        let raised = EventBody::AttentionRaised(AttentionRaised {
            attn_id: new_id(),
            reason: AttentionReason::Question,
            summary,
            options: vec![CONFIRM.into(), CANCEL.into()],
            ext: twr_ext_map(json!({ PROPOSAL_KEY: proposal })),
            ..Default::default()
        });
        if let Err(e) = self.append(hub, HUB_THREAD, None, Source::Hub, raised) {
            self.orch.card = Some(card);
            return Err(e);
        }
        Ok(())
    }

    /// Whether `decision`, an answer to confirm card `item`, confirms it ([`CONFIRM`]) or
    /// cancels it ([`CANCEL`]), as listed or in any case; `bad_request` otherwise.
    pub(super) fn confirms(&self, item: &AttentionItem, decision: &str) -> Result<bool, RpcError> {
        let d = decision.trim();
        if d.eq_ignore_ascii_case(CONFIRM) {
            Ok(true)
        } else if d.eq_ignore_ascii_case(CANCEL) {
            Ok(false)
        } else {
            Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} is a confirm card: answer {CONFIRM} or {CANCEL} (or dismiss it)",
                    item.attn_id
                ),
            ))
        }
    }

    /// The user confirmed card `item` (holding `proposal`): every action is applied as the
    /// user's, each checked again; those that no longer hold are skipped. The card's
    /// resolution (with what was applied and skipped) and every applied action's events are
    /// one batch, so a crash or a write failure leaves either all of it recorded or none of it
    /// (the card still open). Returns the card's `attention.resolved`.
    pub(super) fn confirm_card(
        &mut self,
        hub: &Arc<Inner>,
        item: &AttentionItem,
        proposal: &Proposal,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        let card = item.attn_id.clone();
        // Out of the open cards while it applies: a message an action routes to the
        // orchestrator must not supersede it.
        self.hub_cards.remove(&card);
        let mut batch = Batch::default();
        let mut staged = Vec::new();
        let mut applied = Vec::new();
        let mut skipped = Vec::new();
        let mut earlier = Earlier::default();
        for action in &proposal.actions {
            let what = self.describe(action);
            match self.stage_action(hub, &mut batch, action, caller, &card, &mut earlier) {
                Ok(s) => {
                    staged.push(s);
                    applied.push(what);
                }
                Err(e) => skipped.push(format!("{what} ({})", e.message)),
            }
        }
        let resolved = batch.push(
            HUB_THREAD,
            None,
            caller.source(),
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: card.clone(),
                by: ResolvedBy::User,
                answer: Some(Answer::new(CONFIRM)),
                ext: twr_ext_map(json!({
                    CONFIRMED_KEY: { "applied": applied, "skipped": skipped },
                })),
                ..Default::default()
            }),
        );
        let done = match self.append_batch(hub, batch) {
            Ok(done) => done,
            Err(e) => {
                self.hub_cards.insert(card, proposal.clone());
                return Err(append_err(e));
            }
        };
        // Every answer reaches its runtime first, whatever the order of the actions: a mode
        // change's restart (or an answer's deferred one) must not stop a runtime before it
        // got the answers to its items raised between turns. Then the answers' dispatches and
        // deferred restarts, then the mode changes' restarts and dispatches.
        let answers = || {
            staged.iter().filter_map(|s| match s {
                Staged::Answer(a) => Some(a),
                Staged::Mode(_) | Staged::Move(_) => None,
            })
        };
        for a in answers() {
            self.forward_answer(hub, a);
        }
        for a in answers() {
            self.after_answer(hub, a, &done.queued);
        }
        for s in &staged {
            match s {
                Staged::Mode(m) => {
                    self.finish_mode(hub, m);
                }
                Staged::Move(thread) => {
                    self.apply_settings(hub, thread, &mut None);
                }
                Staged::Answer(_) => {}
            }
        }
        done.events
            .get(resolved)
            .cloned()
            .ok_or_else(|| internal("the confirmation was not recorded"))
    }

    /// Adds `action` of confirm card `card` to `batch` as the user's (`caller`), checked
    /// against the hub as it is now exactly like the user's own answer to that revision or
    /// their own `set_mode`, with the card's `earlier` actions applied: a mode is checked
    /// against the runtime an earlier move takes the thread to, and a move against the mode
    /// an earlier action sets (so Confirm never leaves a thread on a runtime in a mode it does
    /// not take). An error (and `batch` as it was) when it no longer holds.
    fn stage_action(
        &mut self,
        hub: &Arc<Inner>,
        batch: &mut Batch,
        action: &Proposed,
        caller: &Caller,
        card: &str,
        earlier: &mut Earlier,
    ) -> Result<Staged, RpcError> {
        match action {
            Proposed::Answer {
                attn_id,
                revision,
                answer,
                ..
            } => {
                let item = self.open_item(attn_id)?;
                self.check_answer_basis(attn_id, &item, caller, AnswerFor::Revision(*revision))?;
                if self.hub_cards.contains_key(attn_id) {
                    return Err(rpc(
                        codes::BAD_REQUEST,
                        format!("{attn_id} is a confirm card"),
                    ));
                }
                let answer = answer_of(
                    Response::Answer(Answer::new(answer.clone())),
                    &item,
                    attn_id,
                )?;
                self.stage_answer(hub, batch, &item, answer, false, caller, Some(card))
                    .map(Staged::Answer)
            }
            Proposed::Mode {
                thread,
                mode,
                lifecycle,
                runtime,
                acp_command,
            } => {
                let on = earlier.runtimes.get(thread).map(String::as_str);
                let (id, view, canonical) = self.mode_target_on(thread, mode, on)?;
                // The thread the card named, not a later life of it under its id.
                if view.lifecycle_seq != *lifecycle {
                    return Err(rpc(
                        codes::CONFLICT,
                        format!(
                            "{} was deleted and restored since the card was raised",
                            view.name
                        ),
                    ));
                }
                // On the runtime the card's mode was meant for: the orchestrator may have moved
                // it since without a card (a thread in a restricted mode, `hub::model`), and the
                // mode may let the other runtime do more without asking. (An earlier move of
                // this card is not applied yet: the thread is still where it was proposed.)
                if !on_runtime(&view, runtime.as_deref(), acp_command.as_ref()) {
                    return Err(rpc(
                        codes::CONFLICT,
                        format!(
                            "{} moved to another runtime since the card was raised",
                            view.name
                        ),
                    ));
                }
                let staged = self.stage_mode(batch, &id, &view, &canonical, caller, Some(card));
                earlier.modes.insert(id, canonical);
                Ok(Staged::Mode(staged))
            }
            Proposed::Move {
                thread,
                runtime,
                model,
                effort,
                lifecycle,
            } => {
                let view = self.view(thread)?.clone();
                if view.lifecycle_seq != *lifecycle {
                    return Err(rpc(
                        codes::CONFLICT,
                        format!(
                            "{} was deleted and restored since the card was raised",
                            view.name
                        ),
                    ));
                }
                let mode = earlier.modes.get(thread).cloned();
                let (id, target) = self.stage_move(
                    hub,
                    batch,
                    thread,
                    runtime,
                    model.as_deref(),
                    effort.as_deref(),
                    mode.as_deref(),
                    caller,
                    card,
                )?;
                earlier.runtimes.insert(id.clone(), target);
                Ok(Staged::Move(id))
            }
        }
    }

    /// Adds to `batch` (a user message for the orchestrator arriving, from `source`) the
    /// resolution of every open confirm card as [`SUPERSEDED`].
    pub(super) fn supersede_cards(&self, batch: &mut Batch, source: &Source) {
        let mut open: Vec<&String> = self
            .hub_cards
            .keys()
            .filter(|id| self.attention.get(id).is_some())
            .collect();
        open.sort();
        for id in open {
            batch.push(
                HUB_THREAD,
                None,
                source.clone(),
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id: id.clone(),
                    by: ResolvedBy::User,
                    answer: Some(Answer::new(SUPERSEDED)),
                    ..Default::default()
                }),
            );
        }
    }

    /// Bookkeeping for `attention.resolved` `p` (live and on replay): when it closes a confirm
    /// card, the card is forgotten and the orchestrator gets a notice of what the user did (none
    /// for a card a newer message superseded: that message is its next turn).
    pub(super) fn track_card(&mut self, p: &AttentionResolved) {
        let open = self.hub_cards.remove(&p.attn_id).is_some();
        let result = confirmed(p);
        if !open && result.is_none() {
            return;
        }
        let decision = p.answer.as_ref().map_or("", |a| a.decision.as_str());
        let outcome = match result {
            Some((applied, skipped)) => {
                let list = |v: &[String]| {
                    if v.is_empty() {
                        "nothing".to_string()
                    } else {
                        v.join(", ")
                    }
                };
                format!(
                    "the user confirmed it. Applied: {}. Skipped: {}.",
                    list(&applied),
                    list(&skipped)
                )
            }
            None if decision == SUPERSEDED => {
                self.card_outcomes.insert(
                    p.attn_id.clone(),
                    "the card was superseded; nothing was applied.".into(),
                );
                return;
            }
            None => "the user cancelled it; nothing was applied.".to_string(),
        };
        self.card_outcomes
            .insert(p.attn_id.clone(), outcome.clone());
        // The outcome is rendered as a conversation.update in the same transaction.
    }
}
