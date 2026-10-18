//! Draining a box whose runner speaks an older runner protocol that the hub can still serve
//! drain-only ([`twr_core::runner::DRAINABLE_PROTOCOLS`]): after a hub upgrade across a
//! protocol bump, the box's running turns finish before its runtimes stop, instead of being
//! cut. The SSH supervisor's part is in [`crate::ssh_box`] ("Draining an older daemon").
//!
//! **The connection.** Such a runner authenticates as any other (`hub::runners`). Its
//! connection is *legacy*: the hub sends it acks, pings and the drain-safe commands of the
//! sessions already running (`send`, `respond`, `stop`: [`twr_core::runner::Op::drain_safe`];
//! anything else fails `box_offline` at once, [`crate::remote`]), and asks it nothing else (no
//! agents poll, no trash reconciliation; git snapshots and disk readings are skipped). Its
//! frames are recorded as any runner's.
//!
//! **The record.** The box's first legacy connection enters it into `Core::draining_boxes`
//! ([`Drain`], memory only): its protocol, since when, the deadline ([`SwitchTiming::legacy_wait`]
//! later) and the stage. A legacy connection that comes back keeps all three. Only the
//! reconciliation of a connection of the hub's protocol removes the record (`hub::runners`):
//! that ends the drain, clears the gate and dispatches the box's queues. While the record is
//! there, whatever its stage, the box is gated as a box being switched is (`Core::thread_free`:
//! prompts wait; new threads, parks, resumes, deletions, restores, attaches and trims are
//! refused with [`Drain::refusal`]). Answers go through while the stage is `draining`, so a
//! turn that waits on a permission can finish; `thread.stop` always does. A settings restart
//! still stops a runtime (`Core::dispatch_next`): a narrowed permission mode takes effect as
//! anywhere; the runtime starts again on the hub's protocol only.
//!
//! **The stages.**
//! - `draining`: every [`SwitchTiming::legacy_check`] (and whenever the box gets ready or the
//!   drain is cut short) a watcher of the box (one per record, [`watch`]) looks at it. Once the
//!   box is ready on a legacy connection and idle ([`Core::box_drainable`]: no running turn,
//!   runtime task, open item, answer on its way, operation or runtime stop under way; queued
//!   prompts and due retries do not count, they wait for the new runner anyway), or past the
//!   deadline, or cut short (`box.restart`), the hub stops every live runtime of the box for the
//!   upgrade in one step under the core lock ([`Core::begin_legacy_switch`], the stops of
//!   `hub::box_switch`: a stop already under way keeps its own reason). Past the deadline or
//!   cut short, running turns are cut (`interrupted`).
//! - A box the hub dials (ssh or command) that is not ready, with no connection of the hub's
//!   protocol up, falls back to the replace of a daemon the hub cannot serve: once the deadline
//!   passes, once the drain is cut short, or after [`LEGACY_UNSYNCED`] legacy connections that
//!   ended without syncing (one that does not sync within [`SwitchTiming::legacy_sync`] is
//!   closed). The hub records every runtime the log shows on the box as stopped for the upgrade
//!   (`Core::stop_log_runtimes`) and has its supervisor replace the daemon; the replaced
//!   daemon's spool carries the exits, which decode (its protocol is drainable).
//! - `stopping`: the stops are recorded (durable, their reason in memory; a legacy connection
//!   that comes back does not stop again). Once the box's runtimes have exited (or after
//!   [`SwitchTiming::drain`] from the stage's start), a dialed box's supervisor is told to
//!   replace the daemon (`SshBox::drained`) and the session ends; a token box's runner stays
//!   connected, `drained`, until it is updated. A dialed box that is not ready meanwhile (its
//!   connection lost, or one that does not sync) has its daemon replaced once that wait is
//!   over all the same (the stops are not recorded again; the replaced daemon's spool carries
//!   the exits), and again after each such wait while no connection of the hub's protocol
//!   comes. What the watcher chose is done under the same hold of the core lock as its look,
//!   and bound to the connection it looked at (`Hub::drain_act`).
//! - `needs_install`: a dialed box whose drained daemon, or the one its fallback is to replace,
//!   runs the build the hub launches there (a box of another OS or CPU whose configured build is
//!   old): nothing newer to start, and no replace takes (the box's relay keeps a daemon of its
//!   own build). After `twr box install`, a legacy connection drains in one check and is
//!   replaced; a daemon the fallback was to replace is replaced by the first attempt.
//!
//! A hub restart forgets the record: the next hub drains again, with a new deadline.

use std::sync::{Arc, Weak};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::{watch, Notify};
use tokio::time::Instant;
use twr_core::runner::PROTOCOL_VERSION;

use super::{Core, Hub, HubState, Inner};
use crate::remote::RemoteBox;
use crate::ssh_box::SwitchTiming;

/// Legacy connections in a row that end without syncing, after which a dialed box's drain
/// falls back to replacing its daemon (see the module docs).
pub const LEGACY_UNSYNCED: u32 = 3;

/// How far a box's drain is (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    /// Running turns may finish.
    Draining,
    /// The box's runtimes were stopped for the upgrade.
    Stopping,
    /// A token box's runtimes are stopped: its runner is to be updated.
    Drained,
    /// A dialed box's drained daemon runs the build the hub launches there: `twr box install`.
    NeedsInstall,
}

impl Stage {
    /// As `hub.info` says it.
    fn as_str(self) -> &'static str {
        match self {
            Stage::Draining => "draining",
            Stage::Stopping => "stopping",
            Stage::Drained => "drained",
            Stage::NeedsInstall => "needs_install",
        }
    }
}

/// A box being drained (`Core::draining_boxes`, see the module docs).
pub(super) struct Drain {
    /// The older runner protocol its runner speaks.
    protocol: u32,
    /// The hub dials the box (ssh or command), so it can replace its daemon.
    dialed: bool,
    /// When its first legacy connection was welcomed, and the deadline then, for `hub.info`.
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    /// The deadline: running turns are cut from then on.
    deadline: Instant,
    stage: Stage,
    /// When the stage began (the wait for the stopped runtimes' exits counts from it).
    stage_since: Instant,
    /// Legacy connections in a row that ended without syncing.
    unsynced: u32,
    /// A restart of the box's daemon cut the drain short (`box.restart`).
    cut_short: bool,
    /// Wakes the box's watcher ([`watch`]); it ends when the record is gone or another
    /// record (another wake) replaced it.
    wake: Arc<Notify>,
}

impl Drain {
    /// Whether an answer to a runtime of the box goes through: while its turns may finish.
    pub(super) fn takes_answers(&self) -> bool {
        self.stage == Stage::Draining
    }

    /// Why an operation on box `name` is refused while it is drained.
    pub(super) fn refusal(&self, name: &str) -> String {
        let p = self.protocol;
        match (self.dialed, self.stage) {
            (true, Stage::NeedsInstall) => format!(
                "box {name:?} runs an older twr (runner protocol {p}) and needs this hub's build: \
                 run `twr box install {name}`; new prompts wait until then"
            ),
            (true, _) => format!(
                "box {name:?} runs an older twr (runner protocol {p}) that finishes its running \
                 turns before the hub's build replaces it: try again once it is switched (`twr \
                 box status {name}`)"
            ),
            (false, Stage::Drained) => format!(
                "box {name:?} runs an older twr (runner protocol {p}; the hub speaks \
                 {PROTOCOL_VERSION}): its agents were stopped; update twr on the box and restart \
                 its runner (`twr update`, then restart `twr runner`); new prompts wait until then"
            ),
            // (A runner restarted before its agents are stopped reports their ends as crashes.)
            (false, _) => format!(
                "box {name:?} runs an older twr (runner protocol {p}; the hub speaks \
                 {PROTOCOL_VERSION}): its running turns finish, then its agents stop; once `twr \
                 box status {name}` says drained, update twr on the box and restart its runner; \
                 new prompts wait until then"
            ),
        }
    }

    /// `hub.info`'s `drain` of the box (users only).
    pub(super) fn json(&self) -> Value {
        json!({
            "protocol": self.protocol,
            "since": self.since,
            "until": self.until,
            "stage": self.stage.as_str(),
        })
    }
}

/// What the drain's watcher does next, outside the core lock.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Nothing now.
    Wait,
    /// The record is gone (or replaced): the watcher ends.
    Done,
    /// A dialed box's runtimes are stopped and have exited: its supervisor is to replace its
    /// daemon, which runs build `build` and speaks `protocol` (`SshBox::drained`), and the
    /// session of connection `epoch` (the legacy one looked at) ends.
    Replace {
        epoch: u64,
        build: Option<String>,
        protocol: u32,
    },
    /// A dialed box that is not ready falls back (see the module docs): its supervisor
    /// replaces the daemon at once, and the session of connection `epoch` (a legacy one not
    /// ready), if any, ends.
    ForceReplace { epoch: Option<u64> },
}

impl Core {
    /// Box `rbox`'s connection that runs older runner protocol `protocol` (runner version
    /// `version`) was welcomed: the box enters the drain, or stays in it as it was (see the
    /// module docs). A note for the box when the drain begins.
    pub(super) fn enter_drain(
        &mut self,
        hub: &Arc<Inner>,
        rbox: &RemoteBox,
        protocol: u32,
        version: &str,
    ) -> Option<String> {
        let name = rbox.name();
        if let Some(d) = self.draining_boxes.get_mut(name) {
            d.protocol = protocol;
            return None;
        }
        let wait = hub.box_switch.legacy_wait;
        let since = Utc::now();
        let until = chrono::Duration::from_std(wait)
            .ok()
            .and_then(|w| since.checked_add_signed(w))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        let now = Instant::now();
        let wake = Arc::new(Notify::new());
        let dialed = rbox.transport().dialed();
        self.draining_boxes.insert(
            name.to_string(),
            Drain {
                protocol,
                dialed,
                since,
                until,
                deadline: now + wait,
                stage: Stage::Draining,
                stage_since: now,
                unsynced: 0,
                cut_short: false,
                wake: Arc::clone(&wake),
            },
        );
        tokio::spawn(watch(
            Arc::downgrade(hub),
            name.to_string(),
            wake,
            hub.state.subscribe(),
        ));
        tracing::info!(box_name = %name, protocol, version, "the box's runner speaks an older protocol: draining it");
        let who = if dialed { "daemon" } else { "runner" };
        Some(format!(
            "draining: the box's {who} runs twr {version} (runner protocol {protocol}; the hub \
             speaks {PROTOCOL_VERSION}): running turns may finish until {}; new prompts wait",
            until.format("%H:%M UTC")
        ))
    }

    /// A connection of the hub's protocol of box `name` was reconciled: its drain is over.
    /// Whether there was one.
    pub(super) fn end_drain(&mut self, name: &str) -> bool {
        let Some(d) = self.draining_boxes.remove(name) else {
            return false;
        };
        d.wake.notify_one();
        tracing::info!(box_name = %name, "the box's drain is over");
        true
    }

    /// A legacy connection of box `name` was reconciled: its watcher looks at it now, and the
    /// connections that ended without syncing are counted from none again.
    pub(super) fn drain_ready(&mut self, name: &str) {
        if let Some(d) = self.draining_boxes.get_mut(name) {
            d.unsynced = 0;
            d.wake.notify_one();
        }
    }

    /// Box `name`'s current legacy connection ended without syncing: counted (see the module
    /// docs).
    pub(super) fn drain_unsynced(&mut self, name: &str) {
        if let Some(d) = self.draining_boxes.get_mut(name) {
            d.unsynced = d.unsynced.saturating_add(1);
            tracing::info!(box_name = %name, unsynced = d.unsynced, "a legacy connection of the box ended before it synced");
            d.wake.notify_one();
        }
    }

    /// Whether box `name` can be drained now ([`Core::box_idle`] without the queue: queued
    /// prompts and due retries wait for the new runner anyway): `Err` with why not.
    pub(super) fn box_drainable(&self, hub: &Inner, name: &str) -> Result<(), String> {
        if self.shutting_down {
            return Err("the hub is shutting down".into());
        }
        if !self.online_boxes.contains(name) {
            return Err("the box is offline".into());
        }
        self.box_idle(hub, name, false).map(|_| ())
    }

    /// Stops box `name`'s live runtimes for the upgrade, once it is drainable
    /// ([`Core::box_drainable`]) or whatever it runs with `force` (see the module docs), and
    /// sets its drain's stage to `stopping` in the same step. How many runtimes were stopped
    /// and how many of them had a turn running; why not, when nothing was done.
    pub(super) fn begin_legacy_switch(
        &mut self,
        hub: &Inner,
        name: &str,
        force: bool,
    ) -> Result<(usize, usize), String> {
        if !self.draining_boxes.contains_key(name) {
            return Err("the box is not being drained".into());
        }
        if self.shutting_down {
            return Err("the hub is shutting down".into());
        }
        if !force {
            self.box_drainable(hub, name)?;
        }
        let threads: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.box_name == name)
            .filter(|v| self.rt.get(&v.id).is_some_and(|r| r.live.is_some()))
            .map(|v| v.id.clone())
            .collect();
        let stopped = self.stop_box_runtimes(hub, name, threads)?;
        let running = self.running_turns(&stopped);
        let k = stopped.len();
        if let Some(d) = self.draining_boxes.get_mut(name) {
            d.stage = Stage::Stopping;
            d.stage_since = Instant::now();
            d.cut_short = false;
        }
        Ok((k, running))
    }

    /// One look at box `name`'s drain by its watcher (`wake`, see the module docs), under the
    /// core lock: what to do next outside it, and the notes for the box.
    fn drain_step(
        &mut self,
        hub: &Inner,
        name: &str,
        wake: &Arc<Notify>,
        timing: SwitchTiming,
    ) -> (Action, Vec<String>) {
        let mut notes = Vec::new();
        let Some(d) = self.draining_boxes.get(name) else {
            return (Action::Done, notes);
        };
        if !Arc::ptr_eq(&d.wake, wake) {
            return (Action::Done, notes);
        }
        let Some(rbox) = hub.remote(name) else {
            return (Action::Done, notes);
        };
        let (stage, dialed, protocol, cut_short) = (d.stage, d.dialed, d.protocol, d.cut_short);
        let (unsynced, stage_since) = (d.unsynced, d.stage_since);
        let now = Instant::now();
        let late = now >= d.deadline;
        let legacy_up = rbox.legacy().is_some();
        let ready = legacy_up && rbox.is_ready() && self.online_boxes.contains(name);
        let epoch = rbox.epoch();
        // A connection of the hub's protocol is up: its reconciliation ends the drain.
        let current_up = epoch.is_some() && !legacy_up;
        match stage {
            Stage::Draining | Stage::NeedsInstall if ready => {
                // (Nothing runs on a box that needs an install: it is drained at once.)
                let force = late || cut_short || stage == Stage::NeedsInstall;
                match self.begin_legacy_switch(hub, name, force) {
                    Ok((k, t)) => notes.push(match stage {
                        _ if cut_short => format!(
                            "a restart cuts the drain short: stopping {k} agent(s), cutting {t} \
                             running turn(s)"
                        ),
                        Stage::Draining if late => format!(
                            "drain time is up: stopping {k} agent(s), cutting {t} running turn(s)"
                        ),
                        _ => format!("drained: stopping {k} idle agent(s)"),
                    }),
                    Err(why) => {
                        tracing::debug!(box_name = %name, %why, "the box is not drained yet")
                    }
                }
                (Action::Wait, notes)
            }
            Stage::Draining if dialed && !current_up => {
                let unsynced = unsynced >= LEGACY_UNSYNCED;
                if !(late || cut_short || unsynced) {
                    return (Action::Wait, notes);
                }
                let why = if cut_short {
                    "a restart cuts the drain short".to_string()
                } else if late {
                    "drain time is up and its daemon is not ready".to_string()
                } else {
                    format!(
                        "{LEGACY_UNSYNCED} of its daemon's connections ended before they synced"
                    )
                };
                let mut note =
                    format!("{why}: replacing the box's daemon (runner protocol {protocol})");
                match self.stop_log_runtimes(hub, name) {
                    Ok((stopped, running)) => {
                        let k = stopped.len();
                        if k > 0 {
                            note.push_str(&format!(
                                "; its {k} agent(s) stop, and each thread's next prompt resumes \
                                 its session"
                            ));
                        }
                        if running > 0 {
                            note.push_str(&format!(" ({running} running turn(s) cut)"));
                        }
                    }
                    Err(why) => {
                        tracing::warn!(box_name = %name, %why, "the box's runtimes could not be recorded as stopped before its daemon is replaced");
                        note.push_str(
                            "; its agents could not be recorded as stopped (their threads crash)",
                        );
                    }
                }
                if let Some(d) = self.draining_boxes.get_mut(name) {
                    d.stage = Stage::Stopping;
                    d.stage_since = now;
                    d.cut_short = false;
                }
                notes.push(note);
                (Action::ForceReplace { epoch }, notes)
            }
            Stage::Stopping if ready => {
                let drained = self.box_drained(name);
                if !drained && now < stage_since + timing.drain {
                    return (Action::Wait, notes);
                }
                if !drained {
                    tracing::warn!(box_name = %name, "the box's runtimes did not all exit within {} s; replacing its runner all the same", timing.drain.as_secs());
                }
                if dialed {
                    let build = rbox.info().and_then(|i| i.build);
                    let epoch = epoch.expect("a ready connection has an epoch");
                    return (
                        Action::Replace {
                            epoch,
                            build,
                            protocol,
                        },
                        notes,
                    );
                }
                if let Some(d) = self.draining_boxes.get_mut(name) {
                    d.stage = Stage::Drained;
                    d.stage_since = now;
                }
                tracing::info!(box_name = %name, protocol, "the box is drained: its runner waits to be updated");
                (Action::Wait, notes)
            }
            // The stops are recorded, but the box lost its connection, or the one it has does
            // not sync: once the exits' wait is over all the same, a dialed box's daemon is
            // replaced (its spool carries the exits), the stops not recorded again; asked
            // again after each such wait while no connection of the hub's protocol comes.
            Stage::Stopping if dialed && !current_up => {
                if now < stage_since + timing.drain {
                    return (Action::Wait, notes);
                }
                if let Some(d) = self.draining_boxes.get_mut(name) {
                    d.stage_since = now;
                }
                notes.push(format!(
                    "the box's daemon (runner protocol {protocol}) is not ready to report its \
                     stopped agents' ends: replacing it"
                ));
                (Action::ForceReplace { epoch }, notes)
            }
            _ => (Action::Wait, notes),
        }
    }
}

impl Hub {
    /// One look at box `name`'s drain ([`Core::drain_step`]), and what follows from it
    /// ([`Hub::drain_act`], under the same hold of the core lock). Whether the drain goes on.
    fn drain_step(&self, name: &str, wake: &Arc<Notify>) -> bool {
        let timing = self.inner.box_switch;
        let mut core = self.inner.core();
        let (action, notes) = core.drain_step(&self.inner, name, wake, timing);
        self.drain_act(&core, name, action, notes)
    }

    /// Does `action` for box `name`'s drain, with its `notes`, under the core lock (`core`,
    /// held since the look that chose it): the notes, the supervisor told, and the connection
    /// looked at closed, by its epoch. Only while that still holds: the drain's record is there
    /// and, to replace a drained daemon, its legacy connection is the current one, or, to
    /// replace one that is not ready, no connection of the hub's protocol is up. So a
    /// connection that came meanwhile, or the reconciliation of one of the hub's protocol, is
    /// never closed nor taken for the drained daemon. Whether the drain goes on.
    fn drain_act(&self, core: &Core, name: &str, action: Action, notes: Vec<String>) -> bool {
        let sbox = self.inner.ssh_box(name);
        let rbox = self.inner.remote(name);
        // (A box's notes are a leaf lock.)
        for n in notes {
            match &sbox {
                Some(s) => s.remark(n),
                None => tracing::info!(box_name = %name, "{n}"),
            }
        }
        let (Some(sbox), Some(rbox)) = (sbox, rbox) else {
            return !matches!(action, Action::Done);
        };
        let draining = core.draining_boxes.contains_key(name);
        match action {
            Action::Done => return false,
            Action::Wait => {}
            Action::Replace {
                epoch,
                build,
                protocol,
            } => {
                if !(draining && rbox.is_current(epoch) && rbox.legacy().is_some()) {
                    tracing::debug!(box_name = %name, epoch, "the drained connection is gone: no replace");
                } else if sbox.restart_wanted().is_some() {
                    // The restart replaces it (`--restart`).
                    rbox.close_connection_of(
                        epoch,
                        "restarting the box's daemon (`twr box restart`)",
                    );
                } else if sbox.drained(build, protocol) {
                    rbox.close_connection_of(epoch, "replacing the box's older daemon, drained");
                }
            }
            Action::ForceReplace { epoch } => {
                let current_up = rbox.epoch().is_some() && rbox.legacy().is_none();
                if !draining || current_up {
                    tracing::debug!(box_name = %name, "a connection of the hub's protocol is up: no replace");
                } else {
                    sbox.force_replace();
                    if let Some(e) = epoch {
                        rbox.close_connection_of(e, "replacing the box's older daemon");
                    }
                }
            }
        }
        true
    }

    /// Cuts box `name`'s drain short (`box.restart`): its runtimes are stopped at once, cleanly
    /// (see the module docs), before the restart replaces the daemon. Whether the box is being
    /// drained (the drain then ends the box's session itself once they have exited).
    pub(crate) fn cut_drain_short(&self, name: &str) -> bool {
        let wake = {
            let mut core = self.inner.core();
            let Some(d) = core.draining_boxes.get_mut(name) else {
                return false;
            };
            if d.stage == Stage::Draining {
                d.cut_short = true;
            }
            Arc::clone(&d.wake)
        };
        self.drain_step(name, &wake);
        true
    }

    /// Box `name`'s drained daemon runs the build the hub launches there: the box needs an
    /// install (see the module docs).
    pub(crate) fn drain_needs_install(&self, name: &str) {
        if let Some(d) = self.inner.core().draining_boxes.get_mut(name) {
            d.stage = Stage::NeedsInstall;
            d.stage_since = Instant::now();
        }
    }
}

/// The watcher of box `name`'s drain record (`wake`'s): looks at it every
/// [`SwitchTiming::legacy_check`] and whenever woken, until the record is gone or the hub
/// stops.
async fn watch(
    hub: Weak<Inner>,
    name: String,
    wake: Arc<Notify>,
    mut state: watch::Receiver<HubState>,
) {
    loop {
        let Some(inner) = hub.upgrade() else { return };
        let check = inner.box_switch.legacy_check;
        let hub = Hub { inner };
        if *state.borrow_and_update() != HubState::Running || !hub.drain_step(&name, &wake) {
            return;
        }
        drop(hub);
        tokio::select! {
            () = tokio::time::sleep(check) => {}
            () = wake.notified() => {}
            _ = state.wait_for(|s| *s == HubState::Stopped) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;
    use twr_core::config::{BoxConfig, BoxTransport, Config};
    use twr_core::runner::Inventory;

    use super::*;
    use crate::remote::ConnTx;
    use crate::HubOptions;

    /// A connection of `rbox` (of an older protocol: `legacy`), ready and online, under the
    /// core lock as `hub::runners` makes one: its epoch, and its close switch.
    fn connect(
        core: &mut Core,
        rbox: &RemoteBox,
        legacy: Option<u32>,
    ) -> (u64, watch::Receiver<Option<String>>) {
        let (tx, _) = mpsc::channel(8);
        let (close, closed) = watch::channel(None);
        let inv = Inventory {
            build: Some("00000000000000d6".into()),
            ..Default::default()
        };
        let epoch = rbox
            .connect(&inv, ConnTx::new(tx, close), None, legacy)
            .epoch()
            .unwrap();
        assert!(rbox.set_ready(epoch, &[], Vec::new()));
        core.online_boxes.insert(rbox.name().to_string());
        (epoch, closed)
    }

    /// What the drain's watcher chose for a legacy connection is done only while it still
    /// holds: once a connection of the hub's protocol replaced that one and was reconciled
    /// (the drain over), a replace of the drained daemon neither closes the new connection nor
    /// tells the supervisor that a drain replaces its daemon (whose end would then take the
    /// hub's own build for a drained one), and a fallback replace is not asked for.
    #[tokio::test]
    async fn a_stale_drain_action_leaves_a_current_connection_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.boxes.insert(
            "gpu".into(),
            BoxConfig {
                transport: Some(BoxTransport::Ssh),
                ssh: Some("me@gpu.invalid".into()),
                ..Default::default()
            },
        );
        let mut opts = HubOptions::new(dir.path().join("twr.db"), cfg);
        // (The supervisor's attempts never reach the box.)
        opts.box_dialer = Some(Arc::new(|_, _| {
            Box::pin(std::future::pending::<std::io::Result<_>>())
        }));
        let hub = Hub::open(opts).await.unwrap();
        let inner = Arc::clone(&hub.inner);
        let rbox = inner.remote("gpu").unwrap();
        let sbox = inner.ssh_box("gpu").unwrap();
        // The replace of a drained daemon, or the fallback's, for connection `epoch`.
        let action = |replace: bool, epoch: u64| {
            if replace {
                Action::Replace {
                    epoch,
                    build: Some("00000000000000d6".into()),
                    protocol: 6,
                }
            } else {
                Action::ForceReplace { epoch: Some(epoch) }
            }
        };
        for replace in [true, false] {
            let mut core = inner.core();
            let (legacy, _) = connect(&mut core, &rbox, Some(6));
            core.enter_drain(&inner, &rbox, 6, "0.6.0");
            // (Chosen for the legacy connection; then a current one comes and is reconciled.)
            let stale = action(replace, legacy);
            let (_, closed) = connect(&mut core, &rbox, None);
            assert!(core.end_drain("gpu"));
            assert!(hub.drain_act(&core, "gpu", stale, Vec::new()));
            drop(core);
            assert_eq!(*closed.borrow(), None, "replace: {replace}");
            assert_eq!(sbox.take_drain_switch(), None, "replace: {replace}");
            assert!(!sbox.take_replace_asked(), "replace: {replace}");
        }
        // While it holds, it is done.
        for replace in [true, false] {
            let mut core = inner.core();
            let (legacy, closed) = connect(&mut core, &rbox, Some(6));
            core.enter_drain(&inner, &rbox, 6, "0.6.0");
            assert!(hub.drain_act(&core, "gpu", action(replace, legacy), Vec::new()));
            assert!(core.end_drain("gpu"));
            drop(core);
            assert!(closed.borrow().is_some(), "replace: {replace}");
            assert_eq!(sbox.take_drain_switch().is_some(), replace);
            assert_eq!(sbox.take_replace_asked(), !replace);
        }
        hub.shutdown().await;
    }
}
