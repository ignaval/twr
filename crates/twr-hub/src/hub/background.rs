//! A missing terminal notification cannot keep a thread active forever. Background-only
//! work whose task leases expire is stopped through the ordinary, durable stop/reap path.
//! Live inventories renew leases, not stall progress. Settings changes deferred by tasks and
//! automatic retries that are due (`hub::retry`) are dispatched once the tasks are gone; a
//! restart that narrows the mode is not deferred by them (`hub::restart`) and is dispatched
//! whenever it is due. Nothing else is: a crash's queue waits for the user.

use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::Utc;
use twr_core::event::*;

use super::{Caller, Core, HubState, Inner};

impl Core {
    pub(super) fn check_background(&mut self, hub: &Arc<Inner>) -> anyhow::Result<()> {
        if self.shutting_down {
            return Ok(());
        }
        let timeout = chrono::Duration::try_seconds(
            i64::try_from(hub.config.background_timeout_secs).unwrap_or(i64::MAX),
        )
        .unwrap_or(chrono::Duration::MAX);
        let now = Utc::now();
        let expired: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| {
                hub.config.background_timeout_secs > 0
                    && v.current_turn.is_none()
                    && !v.is_blocked()
                    && v.attached.is_none()
            })
            // The runtime's own tasks: a job is the hub's, ended by its own timeout.
            .filter(|v| {
                v.runtime_tasks()
                    && v.background_tasks
                        .values()
                        .filter(|t| !t.job)
                        .all(|t| now.signed_duration_since(t.updated_at) >= timeout)
            })
            .filter(|v| {
                self.box_online(&v.box_name)
                    && !self.rt.get(&v.id).is_some_and(|r| r.stop_requested)
            })
            .map(|v| v.id.clone())
            .collect();
        // The error item is recorded with the stop: a stop recorded without it would close the
        // thread as stopped with nothing saying why, and no later check could tell.
        for thread in expired {
            let why = EventBody::AttentionRaised(AttentionRaised {
                attn_id: new_id(),
                reason: AttentionReason::Error,
                summary: format!("Background task tracking received no liveness signal for {} seconds; stopping the runtime and its tasks. Increase background_timeout_secs for longer silent work.", hub.config.background_timeout_secs),
                options: vec!["dismiss".into()],
                ..Default::default()
            });
            self.stop_thread_with(
                hub,
                &thread,
                &Caller::User("background-timeout".into()),
                Some(why),
            )
            .map_err(anyhow::Error::new)?;
        }
        // Only what the ticker is for: a settings restart that was waiting (on the runtime's
        // tasks, unless it narrows the mode) or an automatic retry that is due (a `transient`
        // failure, `hub::retry`). Not any failed turn: after a crash classified otherwise the
        // queue waits for the user, and a dispatch here would run the next queued prompt.
        let ready: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| {
                v.current_turn.is_none()
                    && ((!v.runtime_tasks() && self.restart_due(&v.id))
                        || self.narrowing_due(&v.id)
                        || (v.retry_eligible() && matches!(self.due_retry(&v.id), Ok(Some(_)))))
            })
            .map(|v| v.id.clone())
            .collect();
        for thread in ready {
            self.try_dispatch(hub, &thread)?;
        }
        // Workers idle after their background work ended (`hub::wake`).
        self.check_wakes(hub);
        Ok(())
    }
}

pub(super) async fn ticker(hub: Weak<Inner>) {
    let Some(inner) = hub.upgrade() else {
        return;
    };
    let mut state = inner.state.subscribe();
    let timeout = inner.config.background_timeout_secs;
    let period = if timeout == 0 {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(timeout)
            .div_f64(4.0)
            .clamp(Duration::from_millis(100), Duration::from_secs(1))
    };
    drop(inner);
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = state.wait_for(|s| *s != HubState::Running) => return,
            _ = tick.tick() => {}
        }
        let Some(inner) = hub.upgrade() else {
            return;
        };
        let result = inner.core().check_background(&inner);
        if let Err(error) = result {
            tracing::warn!(%error, "checking background task liveness failed");
        }
    }
}
