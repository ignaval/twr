//! The control stream's snapshot ([`ClientFold`], `control.subscribe` with `snapshot`): the
//! state the TUI would fold from every control item up to the hub's head, installed at once
//! instead of replaying them ([`AppState::install_snapshot`]). The subscription then brings
//! the items after the head, applied live as after a replay.
//!
//! A snapshot carries the newest decisions only ([`twr_core::client_state::SNAPSHOT_DECISIONS`]):
//! the older ones the decision table keeps are read once the first frame is drawn
//! (`decisions.list`, [`AppState::older_decisions`]) and put before them.

use serde_json::Value;
use twr_core::client_state::{self, ClientFold, SHELL_DONE, STARTED_REFS};
use twr_core::event::Event;
use twr_core::proto::{StreamItem, StreamKind};

use crate::app::{AppState, DecisionRow, Incomplete, DECISIONS_CAP};

impl AppState {
    /// Installs `fold`, the state at control position `head` (the subscription's ack), as
    /// the replay of the items up to it would have left this state; its notification memory
    /// ([`ClientFold::policy`]) is the notifier's to adopt ([`crate::notify::Notifier::adopt`]).
    ///
    /// A view kept for this workspace ([`AppState::restore_view`], before this as before a
    /// replay) is reconciled with what the replay used to fold on top of it: the turns started
    /// from a message's ref, the turns that acted and the `!` commands that ended are its own
    /// followed by the snapshot's newer ones ([`client_state::merge_rings`]); its `!` commands
    /// that ended since (or whose thread is gone) are no longer its to stop; its last message
    /// learns the turn it started, and whether that turn acted or ended. Then the rest of the
    /// view comes back, as at the end of a replay ([`AppState::finish_restore`]).
    pub fn install_snapshot(&mut self, fold: ClientFold, head: u64) {
        let ClientFold {
            registry,
            attention,
            usage,
            policy: _,
            decisions,
            decisions_seen,
            started_refs,
            acted_turns,
            shell_done,
            went_on,
            delete_cards,
            capped,
            last_gseq: _,
        } = fold;
        self.registry = registry;
        self.attention = attention;
        self.usage = usage;

        let rows: Vec<DecisionRow> = decisions
            .iter()
            .filter_map(|(_, ev)| DecisionRow::from_event(ev))
            .collect();
        let kept = rows.len();
        self.decisions.install(rows, decisions_seen);
        // The decisions older than the snapshot's, while the table has room for them.
        self.older_decisions = match decisions.front() {
            Some((oldest, _)) if decisions_seen > kept as u64 && kept < DECISIONS_CAP => {
                let missing = usize::try_from(decisions_seen - kept as u64).unwrap_or(usize::MAX);
                Some((*oldest, (DECISIONS_CAP - kept).min(missing)))
            }
            _ => None,
        };

        self.summary.went_on.extend(went_on);
        self.delete_cards = delete_cards;
        self.incomplete = capped
            .into_iter()
            .map(|(id, (revision, raise))| {
                let raise = StreamItem {
                    stream: StreamKind::Control,
                    gseq: Some(revision),
                    event: raise,
                };
                let i = Incomplete {
                    revision,
                    raise,
                    fetching: false,
                    failed: false,
                    failures: 0,
                    retry_at: None,
                };
                (id, i)
            })
            .collect();

        if self.restoring.is_some() {
            client_state::merge_rings(&mut self.started_refs, &started_refs, STARTED_REFS);
            client_state::merge_rings(&mut self.acted_turns, &acted_turns, STARTED_REFS);
            client_state::merge_rings(&mut self.shell_done, &shell_done, SHELL_DONE);
        } else {
            self.started_refs = started_refs;
            self.acted_turns = acted_turns;
            self.shell_done = shell_done;
        }
        self.reconcile_restored();

        self.last_gseq = self.last_gseq.max(head);
        self.catch_up = None;
        self.catch_up_items = None;
        // What applying the items would have done at their end: the orchestrator's
        // conversation on the main pane, the picker gone once one is chosen, the view kept
        // for this workspace restored.
        self.sync_main();
        if self.view.picker.as_ref().is_some_and(|p| !p.switch)
            && self.registry.list().iter().any(|t| t.is_orchestrator())
        {
            self.view.picker = None;
        }
        self.finish_restore();
        self.ensure_focus();
        self.sync_action();
    }

    /// What the items folded into a snapshot would have changed of a view kept for this
    /// workspace, which followed its `!` commands and its last message: a command whose end the
    /// snapshot remembers ([`AppState::shell_done`]), or that no longer runs (its thread has
    /// no running job of it: the hub records a job running before it answers the call that
    /// started it, so one this view knows of is in the snapshot), ended; the last message's
    /// turn is the one its ref started, which acted when the snapshot says so and ended when
    /// its thread's last turn is that one or a later one and that one is not open.
    fn reconcile_restored(&mut self) {
        let ended: Vec<(String, String)> = self
            .shell_jobs
            .iter()
            .filter(|s| {
                self.shell_done.contains(&s.job)
                    || !client_state::job_running(&self.registry, &s.thread, &s.job)
            })
            .map(|s| (s.thread.clone(), s.job.clone()))
            .collect();
        for (thread, job) in ended {
            self.shell_gone(&thread, &job);
        }

        let Some(s) = self.sent.as_mut() else {
            return;
        };
        let Some(thread) = s.thread.clone() else {
            return;
        };
        // A thread deleted since: its deleted view says how far it got (a replay saw its turn's
        // end as it came).
        let view = self
            .registry
            .get(&thread)
            .or_else(|| self.registry.deleted_view(&thread));
        if let (None, Some(r)) = (s.turn, s.msg_ref) {
            s.turn = self
                .started_refs
                .iter()
                .rev()
                .find(|(t, m, _)| *t == thread && *m == r)
                .map(|(_, _, turn)| *turn)
                // More turns started from a message than the snapshot keeps: the open one may
                // still be this message's.
                .or_else(|| {
                    view.filter(|v| v.current_turn_ref == Some(r))
                        .and_then(|v| v.current_turn)
                });
        }
        let Some(turn) = s.turn else {
            return;
        };
        s.acted |= self
            .acted_turns
            .iter()
            .any(|(t, n)| *t == thread && *n == turn);
        s.ended |= view.is_some_and(|v| {
            v.last_turn.as_ref().is_some_and(|l| l.turn >= turn) && v.current_turn != Some(turn)
        });
    }

    /// What `decisions.list` answered for the decisions older than the snapshot's
    /// ([`AppState::older_decisions`]): put before the kept rows, as many as the table has room
    /// for, never in place of one. The selected row stays selected.
    ///
    /// A page cut short (`more`, while the table has room) asks for the next one, older than
    /// the oldest it had. A decision a view kept across a switch had selected, older than the
    /// snapshot's, is selected again once a page has it, unless the selection moved since
    /// ([`AppState::pending_decision`]).
    pub fn older_decisions_came(&mut self, result: &Value) {
        let items = result["items"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let oldest = items.iter().filter_map(|i| i["gseq"].as_u64()).min();
        let rows: Vec<DecisionRow> = items
            .iter()
            .filter_map(|item| serde_json::from_value::<Event>(item["event"].clone()).ok())
            .filter_map(|ev| DecisionRow::from_event(&ev))
            .collect();
        if rows.is_empty() {
            self.pending_decision = None;
            return;
        }
        // The table lists the newest first, so rows put at its end move none; a selection
        // past its end (drawn on its last row) is kept on that row.
        let sel = self.decision_selected();
        let moved = self
            .pending_decision
            .as_ref()
            .is_some_and(|(_, at)| *at != self.view.decision_sel);
        self.view.decision_sel = sel;
        self.decisions.prepend_older(rows);
        let room = DECISIONS_CAP.saturating_sub(self.decisions.len());
        let next = match (result["more"] == true, oldest) {
            (true, Some(before)) if room > 0 => Some((before, room)),
            _ => None,
        };
        if let Some((id, _)) = self.pending_decision.take().filter(|_| !moved) {
            match self.decision_rows().iter().position(|row| row.id == id) {
                Some(i) => self.view.decision_sel = i,
                None if next.is_some() => {
                    self.pending_decision = Some((id, self.view.decision_sel));
                }
                None => {}
            }
        }
        self.older_decisions = next;
    }
}
