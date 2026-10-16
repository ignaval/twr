//! The hub's part of switching an SSH box to the build it launched there, when the box's
//! daemon runs another build of the same runner protocol (the whole of it, and the
//! supervisor's part, are in [`crate::ssh_box`], "Switching builds"): the quiet check
//! ([`Core::box_quiet`]), the box's `switching` gate (`Core::switching_boxes`, memory only)
//! set in one step with it and with the stops of the box's live runtimes (reason
//! [`BOX_UPGRADE_REASON`]), whether those have exited, and the end of the switch once the
//! box's next connection is reconciled (`hub::runners`).
//!
//! While the gate is set the box's threads are not free (`Core::thread_free`): nothing is
//! dispatched to them and no runtime of theirs starts (the automatic start after a settings
//! restart included, `hub::restart`); their prompts are queued, as for an offline box. The
//! stops go through the stop machinery as a settings restart's do: a durable stop request
//! that keeps the queue, the box's `stop` command, and the exit's closing sequence, a
//! `thread.stopped` with `ext.twr.reason` = [`BOX_UPGRADE_REASON`] (in memory as the
//! runtime's `restart_reason`), which does not stop the thread. A `thread.stop` meanwhile
//! clears that reason and drops the queue: the thread stays stopped.

use std::sync::Arc;

use twr_core::event::BOX_UPGRADE_REASON;

use super::{AdapterScope, Batch, Core, Hub, Inner};

impl Core {
    /// Whether box `name` is quiet enough to be switched (see [`crate::ssh_box`]): the hub is
    /// not shutting down, the box is online and not being switched, no repository change of
    /// it is in flight or unsettled, no operation that passed its gate awaits it
    /// ([`Core::reserve_box`]: a new thread, a deletion, a trash restore, an attach being
    /// asked for), and none of its threads has a running turn, a queued
    /// prompt (except behind a failure that holds the queue: it waits for the user anyway), a
    /// due retry, an open item its runtime raised, background tasks of its runtime, an attach
    /// (granted or asked for), a park, resume, deletion or trim under way, a runtime being
    /// stopped, restarted, exiting or given up on, or an answer on its way to the box. `Ok`
    /// with a mark of what the log holds of the box's threads (it moves with any event of
    /// them), `Err` with why not.
    fn box_quiet(&self, hub: &Inner, name: &str) -> Result<u64, String> {
        if self.shutting_down {
            return Err("the hub is shutting down".into());
        }
        if !self.online_boxes.contains(name) {
            return Err("the box is offline".into());
        }
        if self.switching_boxes.contains(name) {
            return Err("the box is being switched already".into());
        }
        let rbox = hub.remote(name).ok_or("the hub has no such box")?;
        let changes = rbox.repository_changes();
        if changes > 0 {
            return Err(format!(
                "{changes} repository change(s) of the box under way"
            ));
        }
        // A new thread being made, a deletion or restore checked, an attach being asked for:
        // they passed the gate and await the box (`Core::reserve_box`).
        let ops = rbox.ops_in_progress();
        if ops > 0 {
            return Err(format!("{ops} operation(s) on the box under way"));
        }
        let mut mark = 0u64;
        for v in self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.box_name == name)
        {
            let n = &v.name;
            mark = mark.wrapping_add(v.last_seq);
            if let Some(turn) = v.current_turn {
                return Err(format!("turn {turn} of {n} is running"));
            }
            if v.runtime_tasks() {
                return Err(format!("{n}'s runtime has background tasks running"));
            }
            if let Some(c) = &v.attached {
                return Err(format!("{n} is attached by {c}"));
            }
            let rt = self.rt.get(&v.id);
            if let Some(rt) = rt {
                if rt.attach.is_some() {
                    return Err(format!("{n} is being attached"));
                }
                if rt.park.is_some() || rt.resuming || rt.deleting || rt.trimming {
                    return Err(format!("{n} is being parked, resumed, deleted or trimmed"));
                }
                if rt.exiting
                    || rt.stop_requested
                    || rt.restart_reason.is_some()
                    || rt.giving_up()
                    || rt.live.as_ref().is_some_and(|l| l.stop_requested)
                {
                    return Err(format!("{n}'s runtime is being stopped or restarted"));
                }
            }
            if !self
                .open_adapter_attention(&v.id, AdapterScope::All, false)
                .is_empty()
            {
                return Err(format!("{n} waits on an answer"));
            }
            let held = rt.is_some_and(|r| r.queue_held);
            match self.store.pending_count(&v.id) {
                Ok(0) => {}
                Ok(_) if held => {}
                Ok(k) => return Err(format!("{n} has {k} prompt(s) queued")),
                Err(e) => return Err(format!("{n}'s queue cannot be read: {e:#}")),
            }
            match self.due_retry(&v.id) {
                Ok(None) => {}
                Ok(Some(turn)) => return Err(format!("{n} has a retry of turn {turn} due")),
                Err(e) => return Err(format!("{n}'s retry cannot be read: {e:#}")),
            }
            match self.store.unanswered_respond(&v.id) {
                Ok(false) => {}
                Ok(true) => return Err(format!("an answer to {n} is on its way to the box")),
                Err(e) => return Err(format!("{n}'s answers cannot be read: {e:#}")),
            }
        }
        Ok(mark)
    }

    /// Sets box `name`'s `switching` gate when it is quiet with nothing recorded on its
    /// threads since mark `mark` ([`Core::box_quiet`]), and stops its live runtimes (see the
    /// module docs). The stop requests and the boxes' `stop` commands are recorded in one
    /// batch before the gate is set: when that fails nothing changed. The threads stopped,
    /// or why the gate was not set.
    fn begin_box_switch(
        &mut self,
        hub: &Inner,
        name: &str,
        mark: u64,
    ) -> Result<Vec<String>, String> {
        if self.box_quiet(hub, name)? != mark {
            return Err("something happened on the box since it was last checked".into());
        }
        let threads: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.box_name == name)
            .filter(|v| self.rt.get(&v.id).is_some_and(|r| r.live.is_some()))
            .map(|v| v.id.clone())
            .collect();
        let stops: Vec<_> = threads
            .iter()
            .filter_map(|t| self.remote_stop(hub, t))
            .collect();
        if !threads.is_empty() {
            let batch = Batch {
                stop_requests: threads.clone(),
                remote_cmds: stops.clone(),
                ..Batch::default()
            };
            self.append_batch(hub, batch)
                .map_err(|e| format!("the stops cannot be recorded: {e:#}"))?;
        }
        self.switching_boxes.insert(name.to_string());
        if let Some(rbox) = hub.remote(name) {
            for c in &stops {
                rbox.expect_stop(&c.thread, &c.cmd_id);
            }
        }
        for t in &threads {
            let rt = self.rt(t);
            rt.stop_requested = true;
            rt.restart_reason = Some(BOX_UPGRADE_REASON);
            if let Some(live) = rt.live.as_mut() {
                live.stop_requested = true;
                let _ = live.handle.stop();
            }
        }
        Ok(threads)
    }

    /// Whether no thread of box `name` has a runtime live or exiting (its exit not recorded
    /// yet).
    fn box_drained(&self, name: &str) -> bool {
        !self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.box_name == name)
            .any(|v| {
                self.rt
                    .get(&v.id)
                    .is_some_and(|r| r.live.is_some() || r.exiting)
            })
    }
}

impl Hub {
    /// Whether SSH box `name` is quiet enough to be switched ([`Core::box_quiet`]): a mark of
    /// what the log holds of its threads, or why not.
    pub(crate) fn box_switch_quiet(&self, name: &str) -> Result<u64, String> {
        self.inner.core().box_quiet(&self.inner, name)
    }

    /// Sets SSH box `name`'s `switching` gate, under the core lock in the same step that finds
    /// the box quiet with nothing recorded on its threads since `mark` (from
    /// [`Hub::box_switch_quiet`]), and stops its live runtimes (see the module docs). Whether
    /// the gate was set.
    pub(crate) fn begin_box_switch(&self, name: &str, mark: u64) -> bool {
        let begun = self.inner.core().begin_box_switch(&self.inner, name, mark);
        match begun {
            Ok(threads) => {
                tracing::info!(box_name = %name, runtimes = threads.len(), "switching the box to the hub's build: its runtimes are being stopped");
                true
            }
            Err(why) => {
                tracing::debug!(box_name = %name, %why, "the box is not switched yet");
                false
            }
        }
    }

    /// Whether every runtime of box `name` has exited and its exit is recorded.
    pub(crate) fn box_switch_drained(&self, name: &str) -> bool {
        self.inner.core().box_drained(name)
    }

    /// Ends box `name`'s current connection (`why`): the switch's session, once drained.
    pub(crate) fn end_box_session(&self, name: &str, why: &str) {
        if let Some(rbox) = self.inner.remote(name) {
            rbox.close_connection(why);
        }
    }
}

/// Box `name`'s switch is over: its connection on build `build` (as its inventory said) is
/// reconciled and its gate cleared (`hub::runners`); its notes say so.
pub(super) fn ended(hub: &Arc<Inner>, name: &str, build: Option<&str>) {
    let build = build.unwrap_or("unknown");
    tracing::info!(box_name = %name, build, "the box's switch is over");
    if let Some(sbox) = hub.ssh_box(name) {
        sbox.remark(format!("now on build {build}"));
    }
}
