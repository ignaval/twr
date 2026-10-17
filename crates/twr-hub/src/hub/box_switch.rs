//! The hub's part of switching an SSH box to the build it launched there, when the box's
//! daemon runs another build of the same runner protocol (the whole of it, and the
//! supervisor's part, are in [`crate::ssh_box`], "Switching builds"): the quiet check
//! ([`Core::box_quiet`]), the box's `switching` gate (`Core::switching_boxes`, memory only)
//! set in one step with it and with the stops of the box's live runtimes (reason
//! [`BOX_UPGRADE_REASON`]), whether those have exited, and the end of the switch once the
//! box's next connection is reconciled (`hub::runners`).
//!
//! A daemon of another runner protocol, which the hub refused, is replaced at once, without
//! waiting for anything: the hub cannot see what it runs. The supervisor first has the hub set
//! the gate and record the stops of every runtime the log shows on the box, in the same way
//! ([`Core::begin_blind_box_switch`]); the replaced daemon's exits, which the new daemon
//! replays, then close their threads as stopped for the switch instead of crashed, and so does
//! the reconciliation for a runtime the new daemon has nothing of. Its open turn ends
//! `interrupted`.
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

use super::{AdapterScope, Batch, Core, Hub, Inner, ThreadRt};

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
                if rt.stopping() {
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
    /// threads since mark `mark` ([`Core::box_quiet`]), and stops its live runtimes
    /// ([`Core::stop_box_runtimes`]). The threads stopped, or why the gate was not set.
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
        self.stop_box_runtimes(hub, name, threads)
    }

    /// Sets box `name`'s `switching` gate as the hub replaces its daemon, which it refused for
    /// its runtime protocol (see [`crate::ssh_box`], "A daemon of another protocol"), and
    /// stops, for the switch ([`Core::stop_box_runtimes`]), every runtime of the box's threads
    /// the log shows that runs here or was not taken over (after a hub restart, every one:
    /// [`Core::unadopted_remote_runtime`]). The replaced daemon spools their exits and the new
    /// one replays them, or the new daemon's reconciliation finds them gone; either closes
    /// their threads as stopped for the switch, not crashed. The threads stopped and how many
    /// of them have a turn running (the replace cuts it), or why nothing was done.
    fn begin_blind_box_switch(
        &mut self,
        hub: &Inner,
        name: &str,
    ) -> Result<(Vec<String>, usize), String> {
        if self.shutting_down {
            return Err("the hub is shutting down".into());
        }
        let threads: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.box_name == name)
            .filter(|v| {
                self.rt.get(&v.id).is_some_and(|r| r.live.is_some())
                    || self.unadopted_remote_runtime(v)
            })
            .map(|v| v.id.clone())
            .collect();
        let stopped = self.stop_box_runtimes(hub, name, threads)?;
        let running = stopped
            .iter()
            .filter(|t| {
                self.registry
                    .get(t)
                    .is_some_and(|v| v.current_turn.is_some())
            })
            .count();
        Ok((stopped, running))
    }

    /// Stops the runtimes of box `name`'s threads `threads` for a switch of the box (see the
    /// module docs) and sets its `switching` gate. A durable stop request for each thread, and
    /// the box's `stop` command for each whose runtime is live here (the hub's session issues
    /// it), are recorded in one batch before the gate is set: when that fails nothing changed.
    /// A thread whose runtime is being stopped or restarted already, for whatever reason, is
    /// left out: its stop keeps its own reason (a `thread.stop` stays a plain stop and stops
    /// the thread, a settings restart keeps its own). The threads stopped.
    fn stop_box_runtimes(
        &mut self,
        hub: &Inner,
        name: &str,
        threads: Vec<String>,
    ) -> Result<Vec<String>, String> {
        let threads: Vec<String> = threads
            .into_iter()
            .filter(|t| !self.rt.get(t).is_some_and(ThreadRt::stopping))
            .collect();
        let stops: Vec<_> = threads
            .iter()
            .filter(|t| self.rt.get(*t).is_some_and(|r| r.live.is_some()))
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

    /// Sets SSH box `name`'s `switching` gate and records its runtimes as stopped for the
    /// switch, as its supervisor replaces its daemon, which spoke another runner protocol
    /// ([`Core::begin_blind_box_switch`]). How many runtimes were stopped and how many of them
    /// had a turn running; `None` when the stops could not be recorded (the replace goes on:
    /// their threads then close as crashed, as before).
    pub(crate) fn begin_blind_box_switch(&self, name: &str) -> Option<(usize, usize)> {
        let begun = self.inner.core().begin_blind_box_switch(&self.inner, name);
        match begun {
            Ok((threads, running)) => {
                tracing::info!(box_name = %name, runtimes = threads.len(), running, "replacing the box's daemon of another protocol: its runtimes are recorded as stopped for the switch");
                Some((threads.len(), running))
            }
            Err(why) => {
                tracing::warn!(box_name = %name, %why, "the box's runtimes could not be recorded as stopped before its daemon is replaced");
                None
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use twr_core::adapter::AdapterHandle;
    use twr_core::config::{BoxConfig, Config};
    use twr_core::event::*;

    use super::super::{HubOptions, Live};
    use super::*;
    use crate::Caller;

    /// A hub with token box `gpu` and, on it, threads whose log shows a runtime (created and
    /// started), one of them (`live`) with a live runtime here; `idle`'s runtime stopped, and
    /// `local` runs on the hub's own box.
    async fn hub_with_threads(dir: &Path) -> Hub {
        let mut cfg = Config::default();
        cfg.boxes.insert(
            "gpu".into(),
            BoxConfig {
                token: Some("gpu-token-0123456789abcdef".into()),
                ..Default::default()
            },
        );
        let hub = Hub::open(HubOptions::new(dir.join("twr.db"), cfg))
            .await
            .unwrap();
        let inner = Arc::clone(&hub.inner);
        let mut core = inner.core();
        for (t, box_name) in [
            ("live", "gpu"),
            ("unadopted", "gpu"),
            ("user", "gpu"),
            ("restarting", "gpu"),
            ("idle", "gpu"),
            ("local", "local"),
        ] {
            let created = EventBody::ThreadCreated(ThreadCreated {
                name: t.into(),
                box_name: box_name.into(),
                runtime: "fake".into(),
                mode: "auto".into(),
                ..Default::default()
            });
            core.append(&inner, t, None, Source::Hub, created).unwrap();
            let started = EventBody::ThreadStarted(SessionPayload {
                session_id: Some(format!("s-{t}")),
                ..Default::default()
            });
            core.append(&inner, t, None, Source::Hub, started).unwrap();
        }
        let stopped = EventBody::ThreadStopped(SessionPayload::default());
        core.append(&inner, "idle", None, Source::Hub, stopped)
            .unwrap();
        let (handle, _end) = AdapterHandle::pair();
        core.generation += 1;
        let generation = core.generation;
        let log_runtime = core.runtime_gen("live");
        core.rt("live").live = Some(Live {
            handle,
            generation,
            stop_requested: false,
            attach: false,
            park: false,
            _mcp_file: None,
            _workdir: None,
            run: None,
            run_confirmed: false,
            given_up: None,
            log_runtime,
        });
        inner.live_count.send_modify(|n| *n += 1);
        drop(core);
        hub
    }

    /// The switch of a box whose daemon is replaced for its protocol stops every runtime the
    /// log shows there, live here or not taken over, in one step with the gate: when the stops
    /// cannot be recorded, nothing is set. A stop already under way keeps its reason (the
    /// user's stays plain, a settings restart keeps its own), and nothing else is touched.
    #[tokio::test]
    async fn a_blind_switch_stops_what_the_log_runs_and_keeps_stops_under_way() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_threads(dir.path()).await;
        let inner = Arc::clone(&hub.inner);
        {
            let mut core = inner.core();
            let r = core
                .stop_thread(&inner, "user", &Caller::user("test"))
                .unwrap();
            assert_eq!(r["stopping"], true, "{r}");
            let rt = core.rt("restarting");
            rt.stop_requested = true;
            rt.restart_reason = Some(MODE_RESTART_REASON);
        }
        let reasons = |core: &Core| {
            ["live", "unadopted", "user", "restarting", "idle", "local"].map(|t| {
                let rt = core.rt.get(t);
                (
                    rt.is_some_and(|r| r.stop_requested),
                    rt.and_then(|r| r.restart_reason),
                )
            })
        };
        // The threads the box's recorded `stop` commands are for.
        let stop_cmds = |core: &Core| -> Vec<String> {
            let mut t: Vec<String> = core
                .store
                .remote_cmds("gpu")
                .unwrap()
                .into_iter()
                .filter(|c| c.cmd_id.starts_with("stop:"))
                .map(|c| c.thread)
                .collect();
            t.sort();
            t
        };
        let before = [
            (false, None),
            (false, None),
            (true, None),
            (true, Some(MODE_RESTART_REASON)),
            (false, None),
            (false, None),
        ];

        // The stops cannot be recorded: no gate, no stop.
        let db = rusqlite::Connection::open(dir.path().join("twr.db")).unwrap();
        db.execute_batch(
            "CREATE TRIGGER inject BEFORE INSERT ON stop_requests \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
        {
            let mut core = inner.core();
            let e = core.begin_blind_box_switch(&inner, "gpu").unwrap_err();
            assert!(e.contains("the stops cannot be recorded"), "{e}");
            assert!(!core.switching_boxes.contains("gpu"));
            assert_eq!(reasons(&core), before);
            assert_eq!(stop_cmds(&core), ["user"]);
        }
        db.execute_batch("DROP TRIGGER inject").unwrap();

        let mut core = inner.core();
        let (mut stopped, running) = core.begin_blind_box_switch(&inner, "gpu").unwrap();
        stopped.sort();
        assert_eq!(
            (stopped, running),
            (vec!["live".into(), "unadopted".into()], 0)
        );
        assert!(core.switching_boxes.contains("gpu"));
        let up = Some(BOX_UPGRADE_REASON);
        assert_eq!(
            reasons(&core),
            [
                (true, up),
                (true, up),
                (true, None),
                (true, Some(MODE_RESTART_REASON)),
                (false, None),
                (false, None),
            ]
        );
        assert!(core.rt["live"].live.as_ref().unwrap().stop_requested);
        assert_eq!(
            core.store.stop_requests().unwrap(),
            ["live", "unadopted", "user"]
        );
        // The box's `stop` goes to the runtime live here (its session issues it); the one not
        // taken over has none (the replace stops it, and a session taking it over asks again).
        assert_eq!(stop_cmds(&core), ["live", "user"]);

        // Again (another refused attempt): nothing more is stopped.
        let (stopped, _) = core.begin_blind_box_switch(&inner, "gpu").unwrap();
        assert!(stopped.is_empty(), "{stopped:?}");
    }
}
