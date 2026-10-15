//! The stall timer: a thread with an open turn or background tasks of its runtime (a job has
//! its own timeout, and its records are no activity) that is not waiting on the user (no open
//! permission / question) and has had no event for `stall_after` gets one hub-raised
//! `attention.raised{reason: stalled, summary: "No activity for N min"}` for that turn.
//!
//! The item resolves `by: timeout` (activity resumed) as soon as another progress event is appended
//! to that thread, including the turn's end or the runtime's closing sequence: the resolution
//! is part of the batch that records the activity ([`Core::stall_resolutions`]), so a hub that
//! dies in between cannot leave it open. Startup resolves any stalled item whose turn is no
//! longer active (logs written before that). Inventory heartbeats renew task liveness without
//! resetting stalls or repeatedly resolving and raising them. A thread has at
//! most one open stalled item. The check runs on a hub tick every `min(30 s, stall_after / 4)`;
//! `stall_after_secs = 0` turns it off.

use std::sync::Weak;
use std::time::Duration;

use chrono::Utc;
use twr_core::event::*;

use super::{BatchEvent, Core, HubState, Inner, ResolvedBy};

/// Longest tick period.
const TICK_MAX: Duration = Duration::from_secs(30);

/// Shortest tick period.
const TICK_MIN: Duration = Duration::from_millis(10);

/// "No activity for N min" (seconds below a minute).
fn stall_summary(after: Duration) -> String {
    let secs = after.as_secs();
    if secs >= 60 {
        format!("No activity for {} min", secs / 60)
    } else {
        format!("No activity for {} s", secs.max(1))
    }
}

/// The tick period for `after`.
pub(super) fn tick_period(after: Duration) -> Duration {
    (after / 4).clamp(TICK_MIN, TICK_MAX)
}

impl Core {
    /// Raises a stalled item for every running, unblocked thread silent for `after`.
    pub(super) fn check_stalls(&mut self, hub: &Inner, after: Duration) -> anyhow::Result<()> {
        if self.shutting_down {
            return Ok(());
        }
        let Ok(after_chrono) = chrono::Duration::from_std(after) else {
            return Ok(());
        };
        let now = Utc::now();
        let due: Vec<(String, Option<u64>)> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.attached.is_none() && !v.is_blocked())
            .filter(|v| !self.stalled.contains_key(&v.id))
            .filter(|v| v.last_activity.is_some_and(|t| now - t >= after_chrono))
            // A job has its own timeout: only the runtime's own work stalls.
            .filter(|v| v.runtime_work())
            .map(|v| (v.id.clone(), v.current_turn))
            .collect();
        // One independent item per thread: a failure part-way leaves the rest for the next
        // tick.
        for (thread, turn) in due {
            self.append(
                hub,
                &thread,
                turn,
                Source::Hub,
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: new_id(),
                    reason: AttentionReason::Stalled,
                    summary: stall_summary(after),
                    options: vec!["dismiss".into()],
                    ..Default::default()
                }),
            )?;
        }
        Ok(())
    }

    /// Bookkeeping of open stalled items (live and on replay).
    pub(super) fn track_stall(&mut self, ev: &Event) {
        match &ev.body {
            EventBody::AttentionRaised(p)
                if p.reason == AttentionReason::Stalled
                    && ev.source == Source::Hub
                    && ev.thread != HUB_THREAD =>
            {
                self.stalled.insert(ev.thread.clone(), p.attn_id.clone());
            }
            EventBody::AttentionResolved(p) if self.stalled.get(&ev.thread) == Some(&p.attn_id) => {
                self.stalled.remove(&ev.thread);
            }
            _ => {}
        }
    }

    /// The resolutions (`by: timeout`) a batch of `events` implies: activity on a thread with
    /// an open stalled item resolves it, after the batch's own events. The item's own events do
    /// not count, and a batch that resolves the item itself needs none.
    pub(super) fn stall_resolutions(&self, events: &[BatchEvent]) -> Vec<BatchEvent> {
        let mut out: Vec<BatchEvent> = Vec::new();
        for ev in events {
            if is_job_task(&ev.source, &ev.body) {
                continue;
            }
            if matches!(
                ev.body,
                EventBody::ConversationUpdate(_)
                    | EventBody::WorktreeDisk(_)
                    | EventBody::WorktreeTrimmed(_)
                    | EventBody::TaskUpdated(TaskUpdated {
                        heartbeat: true,
                        ..
                    })
            ) {
                continue;
            }
            let Some(attn_id) = self.stalled.get(&ev.thread) else {
                continue;
            };
            if out.iter().any(|r| r.thread == ev.thread) || is_own(&ev.body, attn_id) {
                continue;
            }
            let resolved_here = events.iter().any(|e| {
                e.thread == ev.thread
                    && matches!(&e.body, EventBody::AttentionResolved(p) if p.attn_id == *attn_id)
            });
            if resolved_here {
                continue;
            }
            let open = self.registry.get(&ev.thread).and_then(|v| v.current_turn);
            out.push(BatchEvent {
                thread: ev.thread.clone(),
                turn: ev.turn.or(open),
                source: Source::Hub,
                body: resolution(attn_id),
                raw: None,
            });
        }
        out
    }

    /// Startup: resolves the stalled items of threads with no remaining work (a log from
    /// before the resolution was part of the activity's batch). Each is its own append.
    pub(super) fn repair_stalls(&mut self, hub: &Inner) -> anyhow::Result<()> {
        let stale: Vec<(String, String)> = self
            .stalled
            .iter()
            .filter(|(t, _)| {
                self.registry
                    .get(t.as_str())
                    .is_none_or(|v| !v.runtime_work())
            })
            .map(|(t, a)| (t.clone(), a.clone()))
            .collect();
        for (thread, attn_id) in stale {
            let turn = self
                .registry
                .get(&thread)
                .and_then(|v| v.last_turn.as_ref())
                .map(|t| t.turn);
            self.append(hub, &thread, turn, Source::Hub, resolution(&attn_id))?;
        }
        Ok(())
    }
}

/// Whether `body` is one of the stalled item `attn_id`'s own events.
fn is_own(body: &EventBody, attn_id: &str) -> bool {
    match body {
        EventBody::AttentionRaised(p) => p.attn_id == attn_id,
        EventBody::AttentionAnnotated(p) => p.attn_id == attn_id,
        EventBody::AttentionResolved(p) => p.attn_id == attn_id,
        _ => false,
    }
}

fn resolution(attn_id: &str) -> EventBody {
    EventBody::AttentionResolved(AttentionResolved {
        attn_id: attn_id.to_string(),
        by: ResolvedBy::Timeout,
        answer: None,
        ..Default::default()
    })
}

/// The stall tick task: checks every [`tick_period`] until the hub stops or is dropped.
pub(super) async fn stall_ticker(hub: Weak<Inner>, after: Duration) {
    let mut tick = tokio::time::interval(tick_period(after));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(h) = hub.upgrade() else {
            return;
        };
        if *h.state.borrow() == HubState::Stopped {
            return;
        }
        let r = h.core().check_stalls(&h, after);
        if let Err(e) = r {
            tracing::warn!(error = %format!("{e:#}"), "stall check failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_and_ticks() {
        assert_eq!(
            stall_summary(Duration::from_secs(600)),
            "No activity for 10 min"
        );
        assert_eq!(
            stall_summary(Duration::from_millis(300)),
            "No activity for 1 s"
        );
        assert_eq!(tick_period(Duration::from_secs(600)), TICK_MAX);
        assert_eq!(
            tick_period(Duration::from_millis(400)),
            Duration::from_millis(100)
        );
        assert_eq!(tick_period(Duration::from_millis(1)), TICK_MIN);
    }
}
