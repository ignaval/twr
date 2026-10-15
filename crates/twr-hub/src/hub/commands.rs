//! Execute complete, unambiguous user commands without a model inference.

use super::{append_err, twr_ext_map, AnswerFor, Batch, Caller, Core, Inner, RoutedBy};
use serde_json::{json, Value};
use std::sync::Arc;
use twr_core::event::*;
use twr_core::proto::RpcError;
use twr_decisions::code::command::{self, DirectAction, Scene};

impl Core {
    /// Preflight the entire command under the commit lock. No partial fallback applies.
    pub(super) fn apply_command(
        &mut self,
        hub: &Arc<Inner>,
        orch: &str,
        message: &str,
        seen: u64,
        caller: &Caller,
    ) -> Result<Option<Value>, RpcError> {
        // Names as the message arrives (now), what was waiting as the user had seen it.
        let Some((threads, waiting)) = self.command_context(message, self.gseq, seen) else {
            return Ok(None);
        };
        let scene = Scene {
            message,
            threads: &threads,
            waiting: &waiting,
        };
        // Not a lone answer ("yes": `direct_actions` gives none, it may answer the
        // orchestrator's own question), nor a message a thread made or renamed after what the
        // user saw could have captured a word of: the orchestrator reads those.
        let Some(actions) = command::direct_actions(&scene) else {
            return Ok(None);
        };
        if self.names_unsettled(&scene, seen, self.gseq) {
            return Ok(None);
        }
        let mut batch = Batch::default();
        let mut answers = Vec::new();
        let mut modes = Vec::new();
        let mut descriptions = Vec::new();
        for action in actions {
            match action {
                DirectAction::Answer { attn_id, label } => {
                    let item = match self.open_item(&attn_id) {
                        Ok(i) => i,
                        Err(_) => return Ok(None),
                    };
                    // Only a worker's own permission: never a question, a confirm or delete
                    // card, or anything else the hub raised on its own thread.
                    if self
                        .check_answer_basis(&attn_id, &item, caller, AnswerFor::SeenBy(seen))
                        .is_err()
                        || item.reason != AttentionReason::Permission
                        || item.proposal
                        || item.thread == HUB_THREAD
                        || self.hub_cards.contains_key(&item.attn_id)
                        || self.delete_cards.contains_key(&item.attn_id)
                    {
                        return Ok(None);
                    }
                    let staged = match self.stage_answer(
                        hub,
                        &mut batch,
                        &item,
                        Answer::new(&label),
                        false,
                        caller,
                        None,
                    ) {
                        Ok(s) => s,
                        Err(_) => return Ok(None),
                    };
                    descriptions.push(format!("{}: {label}", self.thread_name(&item.thread)));
                    answers.push(staged);
                }
                DirectAction::Mode { thread, mode } => {
                    // Never a thread made after what the user had seen.
                    if !self.command_mode_seen(&thread, seen)
                        || !self.orch.existed_at(&thread, seen)
                    {
                        return Ok(None);
                    }
                    let (id, view, canonical) = match self.mode_target(&thread, &mode) {
                        Ok(t) => t,
                        Err(_) => return Ok(None),
                    };
                    if canonical != mode {
                        return Ok(None);
                    }
                    descriptions.push(format!("{} → {canonical} mode", view.name));
                    modes.push(self.stage_mode(&mut batch, &id, &view, &canonical, caller, None));
                }
            }
        }
        let decision = batch.push(
            HUB_THREAD,
            None,
            caller.source(),
            EventBody::DecisionMade(DecisionMade {
                site: DecisionSite::Route,
                provider: DecisionProvider::Code,
                options: vec!["command".into()],
                chosen: "command".into(),
                confidence: 1.0,
                ext: twr_ext_map(json!({"message": message, "seen_gseq": seen})),
                ..Default::default()
            }),
        );
        self.mark_user_prompt(
            &mut batch,
            orch,
            &caller.source(),
            Some(RoutedBy::Event(decision)),
        );
        batch.push(
            orch,
            None,
            Source::Hub,
            EventBody::ConversationUpdate(ConversationUpdate {
                text: format!("Applied: {}.", descriptions.join("; ")),
                user_message: Some(message.into()),
                ..Default::default()
            }),
        );
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        // Forward every answer before a mode change can restart its runtime.
        for a in &answers {
            self.forward_answer(hub, a);
        }
        for a in &answers {
            self.after_answer(hub, a, &done.queued);
        }
        let changes = modes
            .iter()
            .map(|m| self.finish_mode(hub, m))
            .collect::<Vec<_>>();
        Ok(Some(
            json!({"action": "command", "thread": orch, "applied": descriptions,
            "mode_changes": changes, "decision_ids": [done.events[decision].id]}),
        ))
    }
}
