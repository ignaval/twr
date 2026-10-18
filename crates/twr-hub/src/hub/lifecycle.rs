//! The hub's own life: [`Hub::open`] (the data-directory lock, replaying the log, closing
//! what the previous hub left running, recovery) and shutdown.

use std::collections::{HashMap, HashSet};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use rusqlite::Connection;
use tokio::sync::{broadcast, mpsc, watch};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::RpcError;
use twr_core::reduce::{AttentionStack, Registry};
use twr_decisions::Ladder;

use super::append::hydrated_from_store;
use super::closing::release_live;
use super::{
    after, boxes, context, git, hook, orch, runtime_spec, stall, trash, Core, Exit, Hub,
    HubOptions, HubState, Inner, GIT_TICK, LOCAL_BOX, LOCK_FILE,
};
use crate::host::{LocalHost, StaleProcess, ThreadHost};
use crate::store::{self, Store};

/// How long [`Hub::open`] retries a busy data-directory lock. A lock released moments ago can
/// still be held by a child process forked in between that has not exec'd yet (it shares the
/// open file description until `exec` closes it).
const LOCK_RETRY: Duration = Duration::from_secs(3);

impl Hub {
    /// Locks the data directory (failing if another hub uses it), opens the database,
    /// rebuilds state by replaying it, kills and closes threads whose runtime outlived the
    /// previous hub, and dispatches queued prompts. Must run inside a tokio runtime.
    pub async fn open(mut opts: HubOptions) -> anyhow::Result<Hub> {
        checked_config(&mut opts.config)?;
        let lock = lock_data_dir(&opts.db_path, None).await?;
        Self::open_locked(opts, lock).await
    }

    /// The daemon claims its data before binding, so workspace operations can see a
    /// startup even before its socket appears. In-process callers use `open` as before.
    pub(crate) async fn open_locked(mut opts: HubOptions, lock: File) -> anyhow::Result<Hub> {
        checked_config(&mut opts.config)?;
        // MCP config files of orchestrators that outlived a previous hub (dead tokens).
        orch::remove_stale_mcp_files(&data_dir_of(&opts.db_path));
        // The hook's policy recalls the replay (a report it already knew of, rewritten later,
        // only revises it), and is fed from the commit path from here on: the past is no news.
        let (feed, hook_rx) = hook::HookFeed::new(&opts.config.notify).unzip();
        let (core, reader) = Core::replay_with(&opts.db_path, feed)?;
        let intents = core.store.spawn_intents()?;
        let cap = opts.broadcast_capacity.max(1);
        let (control_tx, _) = broadcast::channel(cap);
        let (events_tx, _) = broadcast::channel(cap);
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let stall_secs = opts.config.stall_after_secs;
        let limits = {
            let threads = opts.config.threads.clone();
            let (run, ctl) = opts
                .systemd_tools
                .clone()
                .unwrap_or_else(super::limits::default_tools);
            tokio::task::spawn_blocking(move || {
                super::limits::Limits::prepare(&threads, &run, &ctl)
            })
            .await
            .unwrap_or_default()
        };
        let warnings = opts.config.warnings(opts.config_file.as_deref());
        for w in &warnings {
            tracing::warn!("{w}");
        }
        let local = local_host(&opts);
        let job_slots_dir = opts
            .job_slots_dir
            .clone()
            .unwrap_or_else(|| data_dir_of(&opts.db_path).join("job-slots"));
        let inner = Arc::new(Inner {
            core: Mutex::new(core),
            reader: Mutex::new(reader),
            control_tx,
            events_tx,
            live_count: watch::Sender::new(0),
            state: watch::Sender::new(HubState::Running),
            boxes: std::sync::RwLock::new(boxes::Boxes::with_local(local)),
            config: opts.config,
            config_file: opts.config_file,
            change_request_timeout: opts.change_request_timeout,
            box_switch: opts.box_switch,
            box_files_lock: tokio::sync::Mutex::new(()),
            box_files_read_hook: opts.box_files_read_hook,
            socket_removed_hook: opts.socket_removed_hook,
            box_dialer: opts.box_dialer,
            local_agents: Arc::new(super::box_agents::LocalAgents::new(opts.agent_probe)),
            agents_poll: opts.agents_poll,
            worktree_namespace: opts.worktree_namespace,
            listening: Default::default(),
            box_key: Default::default(),
            db_path: opts.db_path,
            instance_name: opts.instance_name,
            workspace: opts.workspace,
            workspaces_root: opts.workspaces_root,
            stop_timeout: opts.stop_timeout,
            hello_timeout: opts.hello_timeout,
            runner_idle_timeout: opts.runner_idle_timeout,
            ack_batch_delay: opts.ack_batch_delay,
            handoff_reply_timeout: opts.handoff_reply_timeout,
            orchestrator_token_unverified: opts.orchestrator_token_unverified,
            marker_read_hook: opts.marker_read_hook,
            peer_confirm_hook: opts.peer_confirm_hook,
            job_gate_hook: opts.job_gate_hook,
            reap_job_group_hook: opts.reap_job_group_hook,
            job_members_hook: opts.job_members_hook,
            started_at: Utc::now(),
            warnings: Mutex::new(warnings),
            fatal: Mutex::new(None),
            ladder: Ladder::standard(),
            socket: opts.socket,
            twr_exe: opts.twr_exe,
            job_gate_exe: opts.job_gate_exe,
            claude_policy: opts.claude_policy,
            notify_tx,
            stall_after: opts.stall_after.unwrap_or(Duration::from_secs(stall_secs)),
            git_tick: opts.git_tick.unwrap_or(GIT_TICK),
            trash_tick: opts.trash_tick.unwrap_or(trash::TRASH_TICK),
            disk_tick: opts.disk_tick.unwrap_or(super::disk::DISK_TICK),
            job_slots_dir,
            test_user_token: opts
                .test_user_token
                .clone()
                .filter(|t| !t.is_empty())
                .map(twr_core::runner::Secret::new),
            looking_until: Default::default(),
            hook_task: Default::default(),
            hook_stop: watch::Sender::new(false),
            limits,
            snapshots: tokio::sync::Mutex::new(()),
            reservations: Default::default(),
            spawn_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            start_ticks: tokio::task::spawn_blocking(twr_runner::reap::own_start_time)
                .await
                .ok()
                .flatten(),
            _lock: lock,
        });
        // Every remote box, before the log's runtimes are looked at: a remote box's are its
        // runner's to report (`stale_runtimes`). All of them were admitted already
        // (`checked_config`); their tasks start below, once the hub is open.
        let mut registered = Vec::new();
        for (name, b) in inner.config.boxes.clone() {
            match boxes::register_box(&inner, &name, b) {
                Ok(r) => registered.push(r),
                Err(e) => anyhow::bail!("box {name}: {}", RpcError::from(e).message),
            }
        }
        // What is left of the jobs a stopped hub ran and did not see end goes before anything
        // can start a runtime or a job in their directories or take their slots (it holds
        // none any more), and before they are recorded lost below: each job's process group as
        // recorded before its command ran, whatever its processes' environments say, then
        // whatever still carries one of their markers (a job recorded before its group was).
        // A job whose leftovers cannot be verified gone is not recorded lost, and its thread
        // starts nothing until a later start verifies them (`Core::hold_unreaped`).
        let lost = inner.core().lost_job_leftovers();
        if !lost.is_empty() {
            let unreaped = reap_lost_jobs(&inner, lost).await;
            inner.core().hold_unreaped(&inner, &unreaped);
        }
        spawn_tasks(&inner, notify_rx);
        if let Some(rx) = hook_rx {
            let task = tokio::spawn(hook::hook_task(
                Arc::downgrade(&inner),
                rx,
                inner.state.subscribe(),
                inner.hook_stop.subscribe(),
            ));
            *inner.hook_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        }
        let stale = stale_runtimes(&inner);
        // Kill survivors and wait for them to exit before anything can resume their session.
        reap_stale_runtimes(&inner, &stale).await?;
        reap_unreported_runtimes(&inner, &intents).await?;
        let running_attaches = running_attaches(&inner).await?;
        inner
            .core()
            .recover_at_open(&inner, &intents, stale, running_attaches)?;
        let hub = Hub { inner };
        // Spawn operations a crash left unfinished are settled from the log before anything can
        // ask for their status.
        hub.recover_spawns()?;
        // What a crash left the log and the local box's trash disagreeing on, before anything
        // else can touch those threads.
        hub.reconcile_boxes(Some(LOCAL_BOX)).await;
        // Jobs a stopped hub ran and did not see end (what is left of them is gone already):
        // recorded lost.
        hub.inner.core().close_lost_jobs(&hub.inner);
        // Prompts waiting for other threads: each checked now, then as those threads move.
        after::start(&hub.inner);
        // The remote boxes' own tasks (an SSH box's supervisor dials it from now on).
        for (rbox, ssh) in registered {
            boxes::start_box(&hub.inner, rbox, ssh);
        }
        Ok(hub)
    }

    /// Current shutdown state.
    pub fn state(&self) -> watch::Receiver<HubState> {
        self.inner.state.subscribe()
    }

    /// Asks whoever runs the hub (the socket server) to shut it down.
    pub fn request_shutdown(&self) {
        self.inner.state.send_if_modified(|s| {
            if *s == HubState::Running {
                *s = HubState::ShutdownRequested;
                true
            } else {
                false
            }
        });
    }

    /// [`HubOptions::socket_removed_hook`], when set: `server::serve` awaits it once the
    /// socket is removed.
    pub(crate) async fn socket_removed(&self) {
        if let Some(hook) = &self.inner.socket_removed_hook {
            hook().await;
        }
    }

    /// Waits, at most `within`, until no operation that changes the repositories or worktrees
    /// of the hub's own box runs any more ([`ThreadHost::changes_done`]): each runs to its end
    /// in a task of its own whatever its caller does, holding its locks and reservations until
    /// then. Whether none runs. A trim (`hub::disk`) counts until its outcome is recorded.
    pub async fn box_changes_done(&self, within: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        let waits: Vec<_> = self
            .inner
            .hosts_list()
            .into_iter()
            .map(|(_, h)| h)
            .filter(|h| !h.is_remote())
            .map(|h| h.changes_done(within))
            .collect();
        let boxes = futures::future::join_all(waits)
            .await
            .into_iter()
            .all(|done| done);
        loop {
            if !self.inner.core().rt.values().any(|r| r.trimming) {
                return boxes;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Graceful shutdown: refuses new work, stops every local runtime (appending the closing
    /// events), waits up to the stop timeout, and ends all subscriptions. Runtimes on remote
    /// boxes keep running: the hub lets go of them (no closing events), their runners spool
    /// what they do, and the next hub takes them over when the runner reconnects.
    pub async fn shutdown(&self) {
        {
            self.inner.core().shutting_down = true;
        }
        // Every job first: each records its end (its process group gone) before the runtimes
        // stop.
        self.end_jobs(Duration::from_secs(15)).await;
        // The notification command too, if one runs: it ends it, its group with it.
        let _ = self.inner.hook_stop.send(true);
        let hook = self
            .inner
            .hook_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(task) = hook {
            let _ = tokio::time::timeout(Duration::from_secs(15), task).await;
        }
        {
            let mut core = self.inner.core();
            let remote: Vec<String> = core
                .registry
                .list()
                .into_iter()
                .filter(|v| self.inner.has_remote(&v.box_name))
                .map(|v| v.id.clone())
                .collect();
            for thread in remote {
                if core
                    .rt
                    .get_mut(&thread)
                    .and_then(|r| r.live.take())
                    .is_some()
                {
                    release_live(&self.inner);
                }
            }
            for rt in core.rt.values_mut() {
                if let Some(live) = rt.live.as_mut() {
                    live.stop_requested = true;
                    let _ = live.handle.stop();
                }
            }
        }
        let mut live = self.inner.live_count.subscribe();
        // (Not kept: the watch guard it holds must not live across the awaits below.)
        let stopped_in_time =
            tokio::time::timeout(self.inner.stop_timeout, live.wait_for(|n| *n == 0))
                .await
                .is_ok();
        if !stopped_in_time {
            // Runtimes that outlived the stop timeout (and their adapters' own kill escalation)
            // are killed first, with their process groups, and only then recorded as stopped:
            // the log never says stopped for a process that may still run (the next hub would
            // no longer look for it).
            let stuck: Vec<(String, u64, Option<StaleProcess>, String)> = {
                let core = self.inner.core();
                core.rt
                    .iter()
                    .filter_map(|(t, r)| {
                        let l = r.live.as_ref()?;
                        // Captured with its generation under the core lock; signalled only
                        // while it still carries that generation's marker.
                        let proc = r.pid.map(|pid| StaleProcess {
                            thread: t.clone(),
                            pid,
                            start_time: r.start_time,
                            exe: r.exe.clone(),
                            generation: Some(l.generation.to_string()),
                        });
                        let box_name = core
                            .registry
                            .get(t)
                            .map_or_else(|| LOCAL_BOX.to_string(), |v| v.box_name.clone());
                        Some((t.clone(), l.generation, proc, box_name))
                    })
                    .collect()
            };
            // Only those confirmed gone are closed: one that still runs keeps its runtime (and
            // the log shows it running), so the next hub start reaps it before anything else.
            let mut gone = HashSet::new();
            for (thread, generation, proc, box_name) in &stuck {
                let Some(host) = self.inner.host(box_name).filter(|h| !h.is_remote()) else {
                    // (Remote runtimes were let go above.)
                    continue;
                };
                tracing::warn!(%thread, "runtime did not stop in time; killing it");
                let (t, proc) = (thread.clone(), proc.clone());
                let (min_start, gen) = (self.inner.start_ticks, Some(*generation));
                let reaped = tokio::task::spawn_blocking(move || {
                    if let Some(p) = &proc {
                        host.reap_stale(p);
                    }
                    if let Some(m) = min_start {
                        host.reap_marked(&t, m, gen);
                    }
                    !host.runtime_running(&t, proc.as_ref(), min_start, gen)
                })
                .await;
                match reaped {
                    Ok(true) => {
                        gone.insert(thread.clone());
                    }
                    Ok(false) => {
                        tracing::error!(%thread, "runtime still runs after being killed; leaving it to the next hub start");
                    }
                    Err(e) => tracing::error!(error = %e, "killing a stuck runtime failed"),
                }
            }
            let mut core = self.inner.core();
            for (thread, generation, _, _) in stuck {
                if !gone.contains(&thread) {
                    continue;
                }
                tracing::warn!(%thread, "runtime did not stop in time; closing it");
                if let Some(mut live) = core.remove_live(&thread, generation) {
                    let attach_client = core.attach_client(&thread, &live);
                    let park = core.park_exit(&thread, &live);
                    let parking = park.is_some();
                    let user_stop = core.rt.get(&thread).is_some_and(|r| r.stop_requested);
                    let exit = match (&attach_client, park, live.given_up.take()) {
                        (Some(client), _, _) => Exit::Attach {
                            client: client.clone(),
                        },
                        (None, Some(park), _) => park,
                        // A runtime the hub gave up on crashed (unless the user stopped it).
                        (None, None, Some(why)) if !user_stop => Exit::Crashed {
                            exit_code: None,
                            stderr_tail: why,
                        },
                        (None, None, _) => Exit::Stopped,
                    };
                    let closed = core.close_process(&self.inner, &thread, exit);
                    if let Err(e) = &closed {
                        tracing::error!(%thread, error = %e, "closing thread failed");
                    }
                    release_live(&self.inner);
                    if attach_client.is_some() {
                        core.attach_granted(&thread, closed.is_ok());
                    } else if parking {
                        core.park_quiesced(&thread, closed.is_ok());
                    }
                }
            }
        }
        self.inner.state.send_replace(HubState::Stopped);
        // The SSH boxes' supervisors end with it, killing their ssh (their boxes' runtimes
        // keep running there, as a token runner's do).
        self.ssh_boxes_ended(Duration::from_secs(2)).await;
    }

    /// Why the hub failed, if it did: the store could not record events the hub had to record
    /// (see [`HubState`]). The server shuts a failed hub down and exits with this error.
    pub fn fatal_error(&self) -> Option<String> {
        self.inner.fatal()
    }

    /// Number of live runtime processes.
    pub fn live_runtimes(&self) -> usize {
        *self.inner.live_count.borrow()
    }
}

/// The configuration [`Hub::open`] runs with: valid, every box admitted
/// ([`boxes::admit_box`]: a valid name, no box token too short to use), `threads.default_mode`
/// in its canonical spelling (or refused).
fn checked_config(config: &mut Config) -> anyhow::Result<()> {
    config.validate()?;
    for (name, b) in &config.boxes {
        // (Its warning is among `Config::warnings` already.)
        boxes::admit_box(name, b)?;
    }
    // `threads.default_mode` in its canonical spelling, or the config is refused.
    config.threads.default_mode = runtime_spec::canonical_mode(&config.threads.default_mode, None)
        .map_err(|e| anyhow::anyhow!("threads.default_mode: {e}"))?;
    Ok(())
}

impl Core {
    /// [`Core::replay_with`] without a notification hook.
    #[cfg(test)]
    pub(super) fn replay(db_path: &Path) -> anyhow::Result<(Core, Connection)> {
        Self::replay_with(db_path, None)
    }

    /// The state the log at `db_path` records: the store, every event folded into the
    /// reducers, and the `thread.stop`s still waiting for their runtime's exit. Returns it with
    /// a reader connection. With the notification hook's feed ([`hook::HookFeed`]), its
    /// policy recalls each replayed control event as it is folded, and gives nothing for them.
    pub(super) fn replay_with(
        db_path: &Path,
        hook: Option<hook::HookFeed>,
    ) -> anyhow::Result<(Core, Connection)> {
        let store = Store::open(db_path)?;
        let reader = store::connect(db_path)?;
        let events = store.load_all_with_gseq()?;
        let gseq = store.control_head()?;
        let mut core = Core {
            store,
            registry: Registry::new(),
            attention: AttentionStack::new(),
            seqs: HashMap::new(),
            last_ts: HashMap::new(),
            gseq,
            known_attn: HashSet::new(),
            adapter_attn: HashMap::new(),
            rt: HashMap::new(),
            generation: 0,
            leases: 0,
            shutting_down: false,
            dropped_events: 0,
            sanitized_events: 0,
            persist_retries: 0,
            orch: orch::OrchState::default(),
            hub_questions: HashMap::new(),
            hub_cards: HashMap::new(),
            delete_cards: HashMap::new(),
            disk: Default::default(),
            jobs: Default::default(),
            hook,
            after: Default::default(),
            card_outcomes: HashMap::new(),
            context_hashes: HashMap::new(),
            usage: Default::default(),
            project_contexts: HashMap::new(),
            context_files: context::FileCache::default(),
            context_roots: HashMap::new(),
            hub_errors: HashMap::new(),
            stalled: HashMap::new(),
            activity: HashMap::new(),
            reserved_names: HashSet::new(),
            trash: Default::default(),
            deleted_rt: HashMap::new(),
            trash_busy: HashSet::new(),
            items_deleted: HashSet::new(),
            items_restored: HashMap::new(),
            online_boxes: HashSet::new(),
            switching_boxes: HashSet::new(),
            draining_boxes: HashMap::new(),
            defer_ack_sync: false,
        };
        // Reducers fold hydrated payloads: control kinds, agent text whose end the thread's
        // activity keeps (`hub::activity`; a capped one has a `text_ref`), and tool calls,
        // whose input and extras its activity line reads (a capped one keeps a preview).
        for (ev, g) in &events {
            if let Some(g) = g {
                core.gseq = *g;
            }
            let folded_whole = ev.is_control()
                || matches!(&ev.body, EventBody::AgentText(t) if !t.extra.is_empty())
                || matches!(&ev.body, EventBody::AgentToolCall(_));
            let full = folded_whole
                .then(|| hydrated_from_store(core.store.conn(), ev))
                .flatten();
            let ev = full.as_ref().unwrap_or(ev);
            core.apply(ev);
            if let Some(g) = g {
                core.recall_hook(ev, *g);
            }
        }
        core.gseq = gseq;
        drop(events);
        // `thread.stop`s whose runtime's exit is not recorded yet (`crate::store`).
        // (Only for threads the log has: a deleted one has none, `hub::delete`.)
        for thread in core.store.stop_requests()? {
            if let Some(rt) = core.rt.get_mut(&thread) {
                rt.stop_requested = true;
            }
        }
        Ok((core, reader))
    }
}

/// The hub's own box (`local`), with the adapters, git and branch namespace of `opts`.
fn local_host(opts: &HubOptions) -> Arc<dyn ThreadHost> {
    let worktrees = data_dir_of(&opts.db_path).join("worktrees");
    let trash = data_dir_of(&opts.db_path).join("trash");
    let host = LocalHost::new(opts.adapters.clone(), worktrees)
        .with_branch_namespace(opts.worktree_namespace.clone())
        .with_trash(trash);
    Arc::new(
        match opts.git_bin.clone() {
            Some(bin) => host.with_git_bin(bin),
            None => host,
        }
        .with_park_disposable(opts.config.threads.park_disposable.clone())
        .with_start_time_faults(opts.start_time_faults),
    )
}

/// Starts the hub's background tasks: the notifier, the stall and git tickers, the hub's own
/// agent probe. (Each remote box's own tasks start with it, `boxes::start_box`.)
fn spawn_tasks(inner: &Arc<Inner>, notify_rx: mpsc::UnboundedReceiver<()>) {
    tokio::spawn(orch::notifier(Arc::downgrade(inner), notify_rx));
    tokio::spawn(super::background::ticker(Arc::downgrade(inner)));
    if !inner.stall_after.is_zero() {
        tokio::spawn(stall::stall_ticker(
            Arc::downgrade(inner),
            inner.stall_after,
        ));
    }
    if !inner.git_tick.is_zero() {
        tokio::spawn(git::git_ticker(Arc::downgrade(inner), inner.git_tick));
    }
    if !inner.trash_tick.is_zero() {
        tokio::spawn(trash::trash_ticker(Arc::downgrade(inner), inner.trash_tick));
    }
    if !inner.disk_tick.is_zero() {
        tokio::spawn(super::disk::disk_ticker(
            Arc::downgrade(inner),
            inner.disk_tick,
        ));
    }
    // The hub's own agent CLIs, in the background (`hub::box_agents`).
    if inner.local_agents.probes() {
        tokio::spawn(super::box_agents::local_ticker(
            Arc::clone(&inner.local_agents),
            inner.state.subscribe(),
            inner.agents_poll.every,
        ));
    }
}

/// A thread whose runtime may have outlived the previous hub: (thread, the recorded process,
/// its box).
type Stale = (String, Option<StaleProcess>, String);

/// Threads whose log shows a live runtime, or a stop request whose closing sequence is
/// not recorded: (thread, the recorded process, its box).
/// Threads of a configured remote box are left alone: their runtime may well still run
/// on the box; their runner's next connection reconciles them (`hub::runners`).
fn stale_runtimes(inner: &Inner) -> Vec<Stale> {
    let core = inner.core();
    core.registry
        .list()
        .into_iter()
        .filter(|v| !inner.has_remote(&v.box_name))
        // A job alone (the hub's, `hub::jobs`) is no runtime left behind.
        .filter(|v| {
            v.current_turn.is_some()
                || v.runtime_tasks()
                || core
                    .rt
                    .get(&v.id)
                    .is_some_and(|r| r.process_live || r.stop_requested)
        })
        .map(|v| {
            let proc = core.rt.get(&v.id).and_then(|r| {
                Some(StaleProcess {
                    thread: v.id.clone(),
                    pid: r.pid?,
                    start_time: r.start_time,
                    exe: r.exe.clone(),
                    // A previous hub's runtime: any generation.
                    generation: None,
                })
            });
            (v.id.clone(), proc, v.box_name.clone())
        })
        .collect()
}

/// Kills the recorded processes of `stale` and waits for them to exit.
async fn reap_stale_runtimes(inner: &Inner, stale: &[Stale]) -> anyhow::Result<()> {
    for (_, proc, box_name) in stale {
        if let (Some(p), Some(host)) = (proc, inner.host(box_name)) {
            let p = p.clone();
            tokio::task::spawn_blocking(move || host.reap_stale(&p)).await?;
        }
    }
    Ok(())
}

/// Runtimes the previous hub started without learning their process (it died first):
/// the processes marked for the thread that started after that hub did.
async fn reap_unreported_runtimes(inner: &Inner, intents: &[(String, u64)]) -> anyhow::Result<()> {
    for (thread, min_start) in intents {
        let box_name = inner
            .core()
            .registry
            .get(thread)
            .map_or_else(|| LOCAL_BOX.to_string(), |v| v.box_name.clone());
        if let Some(host) = inner.host(&box_name).filter(|h| !h.is_remote()) {
            let (t, min_start) = (thread.clone(), *min_start);
            // Any generation: the previous hub's runtimes (this one has started none yet).
            let killed =
                tokio::task::spawn_blocking(move || host.reap_marked(&t, min_start, None)).await?;
            if killed {
                tracing::warn!(%thread, "killed a runtime the previous hub started before it reported its process");
            }
        }
    }
    Ok(())
}

/// Attaches whose interactive process (as its client reported it) still runs: they
/// stay attached until it exits (see Attach). The others end ([`Core::settle_attaches`]).
async fn running_attaches(inner: &Inner) -> anyhow::Result<HashMap<String, store::AttachProcess>> {
    let mut running_attaches: HashMap<String, store::AttachProcess> = HashMap::new();
    let attach_children: Vec<(String, store::AttachProcess)> = {
        let core = inner.core();
        let mut found = Vec::new();
        for v in core.registry.list() {
            if v.attached.is_some() {
                if let Some(c) = core.store.attach_process(&v.id)? {
                    found.push((v.id.clone(), c));
                }
            }
        }
        found
    };
    for (thread, c) in attach_children {
        let probe = c.clone();
        let running = tokio::task::spawn_blocking(move || {
            twr_runner::reap::attach_process_running(
                probe.pid,
                probe.start_time,
                &probe.marker,
                false,
            )
        })
        .await?;
        if running {
            running_attaches.insert(thread, c);
        }
    }
    Ok(running_attaches)
}

impl Core {
    /// What [`Hub::open`] records under the lock once the previous hub's leftovers are killed:
    /// the spawn intents cleared, the adapter items of ended turns resolved, stalled items and
    /// turn reactions repaired, the `stale` threads closed, attaches kept (those of
    /// `running_attaches`) or ended, failed worker turns reacted to, and the queues dispatched.
    fn recover_at_open(
        &mut self,
        inner: &Arc<Inner>,
        intents: &[(String, u64)],
        stale: Vec<Stale>,
        running_attaches: HashMap<String, store::AttachProcess>,
    ) -> anyhow::Result<()> {
        for (thread, _) in intents {
            self.store.clear_spawn_intent(thread)?;
        }
        self.resolve_idle_turn_attention(inner)?;
        // Stalled items left open behind activity by a log from before their resolution
        // was part of the activity's batch.
        self.repair_stalls(inner)?;
        // Turns that ended before the crash but lost their report (and grade, review).
        self.recover_turn_reactions(inner)?;
        self.release_crash_holds()?;
        self.close_stale_runtimes(inner, stale)?;
        self.settle_attaches(inner, running_attaches)?;
        self.react_to_failures_at_open(inner)?;
        for thread in self.store.threads_with_pending()? {
            self.try_dispatch(inner, &thread)?;
        }
        self.autostart_orchestrator(inner);
        // Notices rebuilt from the log wait for the orchestrator like new ones.
        if std::mem::take(&mut self.orch.wake) || !self.orch.notices.is_empty() {
            let _ = inner.notify_tx.send(());
        }
        Ok(())
    }

    /// A turn that ended leaves no request its runtime raised in it open (a crash
    /// between the turn.ended and these resolutions would otherwise keep the thread
    /// waiting). Not on a remote box: its runtime may still run, and a request it
    /// raised between turns (a PTY runtime's question at its prompt) still waits for
    /// the user.
    fn resolve_idle_turn_attention(&mut self, inner: &Inner) -> anyhow::Result<()> {
        let idle: Vec<(String, u64)> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.current_turn.is_none())
            .filter(|v| !inner.has_remote(&v.box_name))
            .filter_map(|v| Some((v.id.clone(), v.last_turn.as_ref()?.turn)))
            .collect();
        for (thread, turn) in idle {
            // The last turn's runtime is the current one (a closed runtime's items were
            // closed with it).
            let gen = self.runtime_gen(&thread);
            self.resolve_turn_adapter_attention(
                inner,
                &thread,
                turn,
                gen,
                ResolvedBy::Timeout,
                true,
            )?;
        }
        Ok(())
    }

    /// Releases the crash holds the log folded ([`ThreadRt::crash_hold`]) that a prompt the
    /// user sent after the crash released before the previous hub stopped: the user's
    /// `thread.send` releases the hold in memory only, and its queue row, with the control
    /// head it was queued at, is what says so (`Store::user_sent_since`; a job's wake or the
    /// orchestrator's send queued since does not count, as it does not live). Without this an
    /// offline box's thread would keep the prompt the user sent after its crash queued for
    /// good.
    ///
    /// [`ThreadRt::crash_hold`]: super::ThreadRt::crash_hold
    fn release_crash_holds(&mut self) -> anyhow::Result<()> {
        let held: Vec<(String, u64)> = self
            .rt
            .iter()
            .filter(|(_, r)| r.queue_held)
            .filter_map(|(t, r)| Some((t.clone(), r.crash_hold?)))
            .collect();
        for (thread, gseq) in held {
            if self.store.user_sent_since(&thread, gseq)? {
                tracing::info!(%thread, "a prompt sent after the thread's crash releases its queue");
                let rt = self.rt(&thread);
                rt.queue_held = false;
                rt.crash_hold = None;
            }
        }
        Ok(())
    }

    /// Closes the `stale` threads (see [`stale_runtimes`]): as stopped when their stop was
    /// asked for, else as crashed with the previous hub.
    fn close_stale_runtimes(&mut self, inner: &Inner, stale: Vec<Stale>) -> anyhow::Result<()> {
        for (thread, _, _) in stale {
            // A runtime the user asked to stop closes as stopped (no error item, no retry).
            let exit = if self.rt.get(&thread).is_some_and(|r| r.stop_requested) {
                tracing::info!(%thread, "closing a stopped thread whose runtime's exit the previous hub did not record");
                Exit::Stopped
            } else {
                tracing::info!(%thread, "closing thread whose runtime died with the previous hub");
                Exit::Crashed {
                    exit_code: None,
                    stderr_tail: "hub restarted; the runtime process is gone".into(),
                }
            };
            self.close_process(inner, &thread, exit)?;
            // The runtime died with the hub, not on its own: its queue goes on (no crash
            // hold, `ThreadRt::queue_held`; a due retry still comes first).
            self.rt(&thread).queue_held = false;
            self.rt(&thread).crash_hold = None;
        }
        Ok(())
    }

    /// Attach leases do not survive the hub: their connections are gone. An attach whose
    /// interactive process still runs is kept (watched until that process exits).
    fn settle_attaches(
        &mut self,
        inner: &Arc<Inner>,
        mut running_attaches: HashMap<String, store::AttachProcess>,
    ) -> anyhow::Result<()> {
        let attached: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.attached.is_some())
            .map(|v| v.id.clone())
            .collect();
        for thread in attached {
            if let Some(child) = running_attaches.remove(&thread) {
                tracing::info!(%thread, pid = child.pid, "keeping a thread attached: its attached process still runs");
                self.adopt_attach(inner, &thread, child);
                continue;
            }
            tracing::info!(%thread, "detaching a thread that was attached when the hub stopped");
            self.detach(inner, &thread, Source::Hub, Some("hub_restart"))?;
        }
        Ok(())
    }

    /// Failed worker turns: the block_kind decision if the previous hub died before
    /// making it, and the automatic retry if it is due and has not started
    /// (`hub::retry`; the retry's turn.started makes this happen once).
    fn react_to_failures_at_open(&mut self, inner: &Arc<Inner>) -> anyhow::Result<()> {
        let workers: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| !v.is_orchestrator())
            .map(|v| v.id.clone())
            .collect();
        for thread in workers {
            self.react_to_failure(inner, &thread)?;
            if self.thread_free(&thread) {
                self.start_retry(inner, &thread)?;
            }
        }
        Ok(())
    }
}

/// Takes the exclusive lock on `<data_dir>/twr.lock` (the directory of `db_path`), retrying a
/// busy lock for up to [`LOCK_RETRY`].
pub(crate) async fn lock_data_dir(db_path: &Path, socket: Option<&Path>) -> anyhow::Result<File> {
    let deadline = tokio::time::Instant::now() + LOCK_RETRY;
    loop {
        match try_lock_data_dir(db_path)? {
            Some(f) => return Ok(f),
            None => {
                // Preserve the immediate same-socket refusal while claiming the data
                // before bind. An absent socket may instead be an ongoing shutdown.
                if let Some(socket) = socket {
                    if matches!(
                        tokio::time::timeout_at(deadline, tokio::net::UnixStream::connect(socket))
                            .await,
                        Ok(Ok(_))
                    ) {
                        anyhow::bail!("a hub is already listening on {}", socket.display());
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    let dir = data_dir_of(db_path);
                    anyhow::bail!("another hub is using {}", dir.display());
                }
                tracing::debug!(db = %db_path.display(), "waiting for data-directory lock");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// Kills what is left of the jobs `lost` (`Core::lost_job_leftovers`): each one's recorded
/// process group ([`twr_runner::reap::reap_job_group`]), then every process still carrying
/// one of their markers. The ids of those whose leftovers cannot be verified gone: a group
/// whose state is unknown or that survived SIGKILL, a marker still carried (or a process table
/// that cannot be read), every one when the cleanup itself failed.
async fn reap_lost_jobs(inner: &Arc<Inner>, lost: Vec<super::jobs::LostJob>) -> Vec<String> {
    use twr_runner::reap::{self, Reaped};
    let hook = inner.reap_job_group_hook.clone();
    let ids: Vec<String> = lost.iter().map(|j| j.id.clone()).collect();
    let done = tokio::task::spawn_blocking(move || {
        let mut unreaped = Vec::new();
        for j in &lost {
            let Some(g) = &j.group else { continue };
            let reaped = match &hook {
                Some(h) => h(g.pgid, g.start_time),
                None => reap::reap_job_group(g.pgid, g.start_time, &g.members),
            };
            if reaped == Reaped::Unknown {
                unreaped.push(j.id.clone());
            }
        }
        let markers: Vec<String> = lost.iter().map(|j| j.marker.clone()).collect();
        reap::reap_agent_marked(&markers);
        match reap::agent_markers_left(&markers) {
            Some(left) => unreaped.extend(
                lost.iter()
                    .filter(|j| left.contains(&j.marker))
                    .map(|j| j.id.clone()),
            ),
            None => unreaped.extend(lost.iter().map(|j| j.id.clone())),
        }
        unreaped.sort();
        unreaped.dedup();
        unreaped
    })
    .await;
    match done {
        Ok(unreaped) => unreaped,
        Err(e) => {
            tracing::error!(error = %e, "cleaning up after the jobs a stopped hub ran failed");
            ids
        }
    }
}

pub(super) fn data_dir_of(db_path: &Path) -> PathBuf {
    match db_path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// One attempt at the data-directory lock: `None` when another open file holds it.
fn try_lock_data_dir(db_path: &Path) -> anyhow::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = data_dir_of(db_path);
    twr_core::paths::ensure_private_dir(&dir)?;
    let path = dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("opening {}: {e}", path.display()))?;
    twr_core::paths::restrict_file(&path)
        .map_err(|e| anyhow::anyhow!("restricting {}: {e}", path.display()))?;
    lock_opened(file, &path)
}

/// Takes the exclusive lock on `file`, opened at `path` (a data directory's lock file):
/// `None` when another open file holds it, or when `file` is no longer the file at `path` by
/// the time it is locked: it went from its path between the open and the lock (`twr ws rm`
/// moves the data directory away, holding the lock), and a lock on it guards nothing that is
/// there now. Callers try again on what the path names.
pub fn lock_opened(file: File, path: &Path) -> anyhow::Result<Option<File>> {
    match file.try_lock() {
        Ok(()) if !locks_path(&file, path) => Ok(None),
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(anyhow::anyhow!("locking {}: {e}", path.display())),
    }
}

/// Whether the open `file` is still the file at `path` (same device and inode).
fn locks_path(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(f), Ok(p)) => f.dev() == p.dev() && f.ino() == p.ino(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::STOP_TIMEOUT;
    use super::*;

    /// A lock on a data directory's lock file that was moved away (by `twr ws rm`) after it
    /// was opened guards nothing: it does not count.
    #[test]
    fn a_lock_on_a_moved_lock_file_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let path = data.join(LOCK_FILE);
        let file = std::fs::File::create(&path).unwrap();
        assert!(locks_path(&file, &path));
        std::fs::rename(&data, tmp.path().join("gone")).unwrap();
        assert!(!locks_path(&file, &path), "nothing at the path");
        std::fs::create_dir(&data).unwrap();
        std::fs::File::create(&path).unwrap();
        assert!(!locks_path(&file, &path), "another file at the path");
        // Opened before the move, locked after it: not taken.
        assert!(lock_opened(file, &path).unwrap().is_none());
        // The real thing takes the new one.
        let db = data.join("twr.db");
        let lock = try_lock_data_dir(&db).unwrap().unwrap();
        assert!(locks_path(&lock, &path));
    }

    #[test]
    fn shutdown_outwaits_every_adapters_own_stop_grace() {
        for grace in [
            twr_adapters::claude_code::STOP_GRACE,
            twr_adapters::codex::STOP_GRACE,
            twr_adapters::grok_build::STOP_GRACE,
            twr_adapters::pty::STOP_GRACE,
        ] {
            assert!(STOP_TIMEOUT >= grace + Duration::from_secs(2), "{grace:?}");
        }
    }

    /// A short box token and a config file other users can access are `hub.info` warnings; a
    /// box token too short to use stops the hub.
    #[tokio::test]
    async fn config_warnings_are_in_hub_info() {
        use super::super::HubOptions;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        // One data directory per hub (each locks its own).
        let db = |name: &str| {
            let d = dir.path().join(name);
            std::fs::create_dir(&d).unwrap();
            d.join("twr.db")
        };
        let config = |token: &str| {
            let mut c = Config::default();
            c.boxes.insert(
                "gpu".into(),
                twr_core::config::BoxConfig {
                    token: Some(token.into()),
                    ..Default::default()
                },
            );
            c
        };
        let mut opts = HubOptions::new(db("short"), config("short-token-1234"));
        opts.config_file = Some(file.clone());
        let hub = Hub::open(opts).await.unwrap();
        let info = hub.info();
        let w: Vec<&str> = info["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(
            w[0].starts_with("boxes.gpu.token is short (16 characters"),
            "{w:?}"
        );
        assert!(
            w[1].contains(&format!("chmod 600 {}", file.display())),
            "{w:?}"
        );
        assert!(!w.concat().contains("short-token"), "{w:?}");
        hub.shutdown().await;
        // A private file and a long token: nothing to say.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut opts = HubOptions::new(db("ok"), config(&"k".repeat(32)));
        opts.config_file = Some(file);
        let hub = Hub::open(opts).await.unwrap();
        assert_eq!(hub.info()["warnings"], serde_json::json!([]));
        hub.shutdown().await;
        // Under 16 characters: the config loads (the CLI reads it for every command), the hub
        // refuses to start.
        let parsed = Config::from_toml_str("[boxes.gpu]\ntoken = \"0123456789\"\n").unwrap();
        for (name, config) in [("bad", config("too-short")), ("bad-file", parsed)] {
            let opts = HubOptions::new(db(name), config);
            let e = Hub::open(opts).await.err().unwrap().to_string();
            assert!(e.contains("boxes.gpu.token is too short"), "{e}");
            assert!(e.contains("`twr runner token` prints a new one"), "{e}");
            assert!(!e.contains("0123456789"), "{e}");
        }
        // A box the hub could never use refuses it too (it was skipped before): an invalid
        // name, `local`.
        for (name, bad, says) in [
            ("bad-name", "a b", "invalid box name \"a b\""),
            ("bad-dash", "-gpu", "invalid box name \"-gpu\""),
            ("bad-local", "local", "the hub's own box"),
        ] {
            let mut c = Config::default();
            c.boxes.insert(
                bad.into(),
                twr_core::config::BoxConfig {
                    token: Some("k".repeat(32)),
                    ..Default::default()
                },
            );
            let e = Hub::open(HubOptions::new(db(name), c))
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(e.contains(says), "{e}");
        }
    }
}
