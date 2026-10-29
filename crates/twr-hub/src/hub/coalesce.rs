//! Fewer `task.updated` records: which of a runtime's background-task updates the hub records
//! ([`Core::task_update_due`], from `Core::record_adapter_event`, for local adapters and
//! runner frames alike; a frame whose update is not recorded still commits its ack).
//!
//! Adapters already drop repeats (`twr_adapters::background`): an unchanged inventory renews a
//! task once a minute (`heartbeat: true`), and progress comes at most once a second per task.
//! The hub records less than that, whatever the producer's version:
//!
//! - always: a task's end, an update with `ext` (except one that only says where its task runs,
//!   as the view already holds it: `ext.twr.foreground_call` / `ext.twr.detached`, which every
//!   update of such a task carries), and an update for a task its thread's view does not hold
//!   (its start, or one the view dropped: a stop cut it, say);
//! - any update when the task's last recorded update is [`heartbeat_every`] old (10 min; a
//!   quarter of `background_timeout_secs` when that is shorter): its lease is renewed in time
//!   whatever the update says. A heartbeat (`heartbeat: true`, saying the description the view
//!   has, or none) only then. This reads only the view, which the log builds, so a restarted
//!   hub decides the same;
//! - a heartbeat with a new description: also when the task's last recorded update is
//!   [`progress_every`] old. It stays a heartbeat: no activity, and no progress here;
//! - progress (`heartbeat: false`): also when the task's last recorded progress is
//!   [`progress_every`] old (30 s; a tenth of the hung-turn threshold when that is shorter).
//!   The time of the last recorded progress is memory only: after a restart the first
//!   progress of each task is recorded, one more than a hub that ran on would have.
//!
//! Times are the events' own (a runner frame's capture time, as the log records it). A
//! progress that is not recorded leaves the thread's `last_activity` at most
//! [`progress_every`] behind, and its task's description until the next update recorded (a
//! heartbeat carrying the new description is one). The checks that read them have room for
//! that: a stall needs `stall_after` of silence and a hung turn `hung_after`, ten times the
//! interval at least, so the first progress after either is always recorded and resolves it
//! (only progress counts as the task's progress, as only progress is activity); the background
//! timeout is four times the heartbeat interval at least. The auto-wake and the
//! orchestrator's idle watches only act on an update for a task the view does not hold (a
//! thread with a task running is neither idle nor due a wake), which is always recorded.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use twr_core::event::{Event, EventBody, TaskUpdated};
use twr_core::reduce::BackgroundTask;

use super::{Core, Inner};

/// The longest a running task's heartbeats go unrecorded.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(10 * 60);

/// The longest a running task's progress goes unrecorded.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(30);

/// How long a task's heartbeats go unrecorded with background timeout `timeout` (zero: off).
pub fn heartbeat_every(timeout: Duration) -> Duration {
    if timeout.is_zero() {
        HEARTBEAT_EVERY
    } else {
        HEARTBEAT_EVERY.min(timeout / 4)
    }
}

/// How long a task's progress goes unrecorded with stall threshold `stall_after` (zero: off).
pub fn progress_every(stall_after: Duration) -> Duration {
    if stall_after.is_zero() {
        PROGRESS_EVERY
    } else {
        PROGRESS_EVERY.min(super::stall::hung_after(stall_after) / 10)
    }
}

/// When each running task's progress was last recorded, by thread and task (memory only).
#[derive(Debug, Default)]
pub(super) struct TaskProgress(HashMap<String, HashMap<String, DateTime<Utc>>>);

/// What [`Core::task_update_due`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Due {
    /// Recorded; `progress`: it counts as the task's progress ([`Core::task_update_recorded`]).
    Record { progress: bool },
    /// Not recorded.
    Skip,
}

/// Whether task update `p`, at `at`, is recorded: `task` is the task as its thread's view holds
/// it, `last` when its progress was last recorded, `every` the progress and heartbeat
/// intervals ([`progress_every`], [`heartbeat_every`]). See the module docs.
pub(super) fn due(
    task: Option<&BackgroundTask>,
    last: Option<DateTime<Utc>>,
    p: &TaskUpdated,
    at: DateTime<Utc>,
    (progress, heartbeat): (Duration, Duration),
) -> Due {
    let Some(task) = task.filter(|t| {
        p.running
            && !t.job
            && twr_core::event::task_ext_unchanged(p, t.foreground_call.as_deref(), t.detached)
    }) else {
        // (A heartbeat is no progress, `ext` or not: it moves no activity in the reducers.)
        return Due::Record {
            progress: p.running && !p.heartbeat,
        };
    };
    let since = |t: DateTime<Utc>| (at - t).to_std().unwrap_or(Duration::ZERO);
    // Whatever it says, an update renews the task's lease once that is due.
    let renews = since(task.updated_at) >= heartbeat;
    let recorded = if !p.heartbeat {
        renews || last.is_none_or(|t| since(t) >= progress)
    } else if p.description.is_empty() || p.description == task.description {
        renews
    } else {
        // A heartbeat saying what the task does now: once per progress interval after the
        // task's last recorded update, and no progress of its own.
        renews || since(task.updated_at) >= progress
    };
    if recorded {
        Due::Record {
            progress: !p.heartbeat,
        }
    } else {
        Due::Skip
    }
}

impl Core {
    /// Whether runtime task update `p` of `thread`, at `at`, is recorded ([`due`]).
    pub(super) fn task_update_due(
        &self,
        hub: &Inner,
        thread: &str,
        p: &TaskUpdated,
        at: DateTime<Utc>,
    ) -> Due {
        let task = self
            .registry
            .get(thread)
            .and_then(|v| v.background_tasks.get(&p.task_id));
        let last = self
            .task_progress
            .0
            .get(thread)
            .and_then(|m| m.get(&p.task_id))
            .copied();
        let every = (
            progress_every(hub.stall_after),
            heartbeat_every(Duration::from_secs(hub.config.background_timeout_secs)),
        );
        due(task, last, p, at, every)
    }

    /// Notes that an update of task `task` of `thread` (`running` or its end), at `at`, was
    /// recorded as `due` said.
    pub(super) fn task_update_recorded(
        &mut self,
        thread: &str,
        (task, running): (&str, bool),
        at: DateTime<Utc>,
        due: Due,
    ) {
        if !running {
            if let Some(m) = self.task_progress.0.get_mut(thread) {
                m.remove(task);
                if m.is_empty() {
                    self.task_progress.0.remove(thread);
                }
            }
            return;
        }
        if due != (Due::Record { progress: true }) {
            return;
        }
        let tasks = self.registry.get(thread).map(|v| &v.background_tasks);
        let m = self.task_progress.0.entry(thread.to_string()).or_default();
        if !m.contains_key(task) {
            m.retain(|id, _| tasks.is_some_and(|t| t.contains_key(id)));
        }
        m.insert(task.to_string(), at);
    }

    /// After recorded event `ev` (live only, never on replay): a lifecycle event that may have
    /// cut its thread's tasks (a runtime start, stop, crash, park or attach) or deleted the
    /// thread forgets the progress of the tasks its view no longer holds.
    pub(super) fn task_progress_after(&mut self, ev: &Event) {
        if !matches!(
            ev.body,
            EventBody::ThreadStarted(_)
                | EventBody::ThreadStopped(_)
                | EventBody::ThreadCrashed(_)
                | EventBody::ThreadParked(_)
                | EventBody::ThreadAttached(_)
                | EventBody::ThreadDeleted(_)
        ) {
            return;
        }
        let Some(m) = self.task_progress.0.get_mut(&ev.thread) else {
            return;
        };
        let tasks = self.registry.get(&ev.thread).map(|v| &v.background_tasks);
        m.retain(|id, _| tasks.is_some_and(|t| t.contains_key(id)));
        if m.is_empty() {
            self.task_progress.0.remove(&ev.thread);
        }
    }

    /// The tasks of `thread` whose progress is remembered (tests).
    #[cfg(test)]
    pub(super) fn remembered_progress(&self, thread: &str) -> usize {
        self.task_progress.0.get(thread).map_or(0, HashMap::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::config::Config;
    use twr_core::event::{Source, ThreadCreated};

    use super::super::{Hub, HubOptions};

    /// The progress remembered of a thread's tasks goes when a lifecycle event cuts them (a
    /// stop) and when the thread is deleted; a job, which outlives its runtime, is no runtime
    /// task (never remembered).
    #[tokio::test]
    async fn remembered_progress_goes_with_the_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        let inner = &hub.inner;
        let mut core = inner.core();
        let at = Utc::now();
        let progress = Due::Record { progress: true };
        for t in ["t1", "t2"] {
            let created = EventBody::ThreadCreated(ThreadCreated {
                name: t.into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                ..Default::default()
            });
            core.append(inner, t, None, Source::User("test".into()), created)
                .unwrap();
            for task in ["a", "b"] {
                let start = EventBody::TaskUpdated(TaskUpdated {
                    task_id: task.into(),
                    running: true,
                    ..Default::default()
                });
                core.append(inner, t, None, Source::Adapter("fake".into()), start)
                    .unwrap();
                core.task_update_recorded(t, (task, true), at, progress);
            }
            assert_eq!(core.remembered_progress(t), 2);
        }
        core.append(
            inner,
            "t1",
            None,
            Source::Hub,
            EventBody::ThreadStopped(Default::default()),
        )
        .unwrap();
        assert_eq!(core.remembered_progress("t1"), 0);
        assert_eq!(core.remembered_progress("t2"), 2);
        core.append(
            inner,
            "t2",
            None,
            Source::Hub,
            EventBody::ThreadDeleted(Default::default()),
        )
        .unwrap();
        assert_eq!(core.remembered_progress("t2"), 0);
        assert!(core.task_progress.0.is_empty());
    }

    /// What is recorded of each kind of update, by the time since the task's last recorded
    /// update (`updated_at`) and progress (`last`).
    #[test]
    fn starts_ends_and_ext_always_progress_and_heartbeats_when_due() {
        let t0 = DateTime::parse_from_rfc3339("2026-10-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let secs = |s: i64| t0 + chrono::Duration::seconds(s);
        let task = BackgroundTask {
            description: "build".into(),
            updated_at: t0,
            started_seq: 1,
            job: false,
            group: None,
            quiet: false,
            shell: false,
            started_at: None,
            foreground_call: None,
            detached: false,
        };
        let every = (PROGRESS_EVERY, HEARTBEAT_EVERY);
        let update = |running: bool, description: &str, heartbeat: bool| TaskUpdated {
            task_id: "b1".into(),
            running,
            description: description.into(),
            heartbeat,
            ..Default::default()
        };
        let progress = Due::Record { progress: true };
        let beat = Due::Record { progress: false };
        let p = update(true, "step 2", false);
        // A start: the view does not hold the task (yet, or any more).
        assert_eq!(due(None, Some(t0), &p, secs(1), every), progress);
        // Progress: once per interval after the last recorded progress, and once after a
        // restart (nothing remembered).
        assert_eq!(due(Some(&task), Some(t0), &p, secs(29), every), Due::Skip);
        assert_eq!(due(Some(&task), Some(t0), &p, secs(30), every), progress);
        assert_eq!(due(Some(&task), None, &p, secs(1), every), progress);
        // A heartbeat: once per interval after the task's last recorded update, whatever
        // progress said; with a new description it is progress.
        let h = update(true, "", true);
        assert_eq!(due(Some(&task), None, &h, secs(599), every), Due::Skip);
        assert_eq!(due(Some(&task), None, &h, secs(600), every), beat);
        let same = update(true, "build", true);
        assert_eq!(due(Some(&task), None, &same, secs(599), every), Due::Skip);
        // A heartbeat with a new description: once per progress interval after the task's
        // last recorded update, never as progress (it is no activity).
        let new = update(true, "step 3", true);
        assert_eq!(due(Some(&task), Some(t0), &new, secs(5), every), Due::Skip);
        assert_eq!(due(Some(&task), Some(t0), &new, secs(30), every), beat);
        assert_eq!(due(Some(&task), None, &new, secs(5), every), Due::Skip);
        // Progress renews the lease when that is due, whatever the progress interval.
        let lease = (PROGRESS_EVERY, Duration::from_secs(2));
        assert_eq!(
            due(Some(&task), Some(secs(1)), &p, secs(2), lease),
            progress
        );
        assert_eq!(
            due(Some(&task), Some(secs(1)), &p, secs(1), lease),
            Due::Skip
        );
        // Always: an end, an update with `ext`, and anything of a job's task.
        let end = update(false, "build", false);
        assert_eq!(due(Some(&task), Some(t0), &end, t0, every), beat);
        let mut ext = update(true, "build", true);
        ext.ext = Some(Default::default());
        assert_eq!(due(Some(&task), Some(t0), &ext, t0, every), beat);
        ext.heartbeat = false;
        assert_eq!(due(Some(&task), Some(t0), &ext, t0, every), progress);
        // An `ext` that only says where the task runs, as the view holds it, is no news: a
        // heartbeat of a foreground task coalesces; a move to the background is recorded.
        let mut placed = update(true, "", true);
        twr_core::event::set_twr_ext(&mut placed.ext, "foreground_call", "toolu_1".into());
        let foreground = BackgroundTask {
            foreground_call: Some("toolu_1".into()),
            ..task.clone()
        };
        assert_eq!(
            due(Some(&foreground), None, &placed, secs(599), every),
            Due::Skip
        );
        assert_eq!(due(Some(&task), None, &placed, secs(1), every), beat);
        // An update with no `ext` at all says it runs in neither place: news for a task the
        // view holds in the foreground.
        let bare = update(true, "build", true);
        assert_eq!(due(Some(&foreground), None, &bare, secs(1), every), beat);
        assert_eq!(due(Some(&task), None, &bare, secs(1), every), Due::Skip);
        let mut moved = update(true, "", false);
        twr_core::event::set_twr_ext(&mut moved.ext, "detached", true.into());
        assert_eq!(
            due(Some(&foreground), Some(t0), &moved, t0, every),
            progress
        );
        let job = BackgroundTask {
            job: true,
            ..task.clone()
        };
        assert_eq!(due(Some(&job), Some(t0), &h, t0, every), beat);
        // A clock that went back counts as no time.
        assert_eq!(due(Some(&task), Some(t0), &p, secs(-5), every), Due::Skip);
    }

    #[test]
    fn intervals_follow_the_thresholds() {
        assert_eq!(heartbeat_every(Duration::ZERO), HEARTBEAT_EVERY);
        assert_eq!(
            heartbeat_every(Duration::from_secs(86_400)),
            HEARTBEAT_EVERY
        );
        assert_eq!(
            heartbeat_every(Duration::from_secs(120)),
            Duration::from_secs(30)
        );
        assert_eq!(progress_every(Duration::ZERO), PROGRESS_EVERY);
        assert_eq!(progress_every(Duration::from_secs(600)), PROGRESS_EVERY);
        assert_eq!(
            progress_every(Duration::from_secs(60)),
            Duration::from_secs(6)
        );
    }
}
