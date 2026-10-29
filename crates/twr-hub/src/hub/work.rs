//! What each thread has in flight ([`twr_core::work::Work`]): folded from its events into its
//! [`super::activity::Activity`] ([`WorkFold`], so a replay at startup rebuilds it), and read
//! with the registry's view, the hub's jobs and the queue when asked ([`Core::work`]). It is
//! the `work` of `thread.get`, `thread.list` and `thread.activity`, and what the hung-turn
//! check judges by (`hub::stall`). Nothing here is recorded.
//!
//! The fold keeps, per thread, the open turn's tool calls without a result (oldest first, at
//! most [`CALLS_KEPT`]), when the turn started, when its runtime last made progress (an event
//! of its adapter that is no heartbeat) and was last seen alive (a heartbeat), each runtime
//! task's latest start or progress, and the latest of its background tasks to end in the open
//! turn. A turn's start or end, and the end of its runtime's process (a stop, crash, park,
//! attach or new start), close the calls.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use twr_core::event::*;
use twr_core::reduce::ThreadView;
use twr_core::work::{self, Call, Ended, Queued, Sight, Task, TaskKind, Work};

use super::Core;

/// Most open calls a thread's fold keeps (the oldest go first).
pub(super) const CALLS_KEPT: usize = 32;

/// Most tasks a thread's fold keeps the progress of.
const TASKS_KEPT: usize = 256;

/// What the log says a thread has in flight (see the module docs).
#[derive(PartialEq, Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct WorkFold {
    /// The open turn's calls without a result, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    calls: Vec<OpenCall>,
    /// When the open (or last) turn started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_at: Option<DateTime<Utc>>,
    /// When its runtime last made progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress_at: Option<DateTime<Utc>>,
    /// When a heartbeat last said its runtime's process is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alive_at: Option<DateTime<Utc>>,
    /// Its running tasks (runtime and jobs), by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tasks: BTreeMap<String, TaskFold>,
    /// The latest background task of its runtime to end in the open turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ended: Option<Ended>,
}

/// An open tool call ([`WorkFold::calls`]).
#[derive(PartialEq, Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OpenCall {
    id: String,
    tool: String,
    what: String,
    since: DateTime<Utc>,
    /// The task it blocks on (`ext.twr.waits_for`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    waits_for: Option<String>,
}

/// A running task's progress ([`WorkFold::tasks`]).
#[derive(PartialEq, Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TaskFold {
    /// Its latest start or progress.
    progress_at: DateTime<Utc>,
    /// What it does, as last said.
    what: String,
    /// Its runtime's background work, as its latest update said (not a foreground or
    /// detached task, nor a job).
    background: bool,
}

/// `tasks` (oldest first) cut to [`work::LISTED_MAX`], keeping first what the reading stands
/// on ([`Work::running_work`]: background tasks, jobs, detached tasks a call waits for again),
/// then the oldest of the rest; oldest first still.
fn keep_listed(tasks: &mut Vec<Task>) {
    if tasks.len() <= work::LISTED_MAX {
        return;
    }
    let runs = |t: &Task| {
        matches!(t.kind, TaskKind::Background | TaskKind::Job)
            || (t.kind == TaskKind::Detached && t.awaited)
    };
    // (A stable sort: each part stays oldest first.)
    tasks.sort_by_key(|t| !runs(t));
    tasks.truncate(work::LISTED_MAX);
    tasks.sort_by_key(|t| (t.since.unwrap_or(t.alive_at), t.id.clone()));
}

impl Core {
    /// Folds `ev` into its thread's [`WorkFold`] (from `Core::track_activity`, after the
    /// registry: the view holds the task as this update left it).
    pub(super) fn track_work(&mut self, ev: &Event) {
        let adapter = matches!(ev.source, Source::Adapter(_));
        let heartbeat = matches!(&ev.body, EventBody::TaskUpdated(p) if p.heartbeat);
        let view_task = match &ev.body {
            EventBody::TaskUpdated(p) => self
                .registry
                .get(&ev.thread)
                .and_then(|v| v.background_tasks.get(&p.task_id))
                .map(|t| {
                    (
                        t.description.clone(),
                        !t.job && t.in_background() && !t.detached,
                    )
                }),
            _ => None,
        };
        let open = self.registry.get(&ev.thread).and_then(|v| v.current_turn);
        let w = &mut self.activity.entry(ev.thread.clone()).or_default().work;
        if adapter && heartbeat {
            w.alive_at = Some(ev.ts);
        } else if adapter
            && !matches!(
                ev.body,
                EventBody::ConversationUpdate(_) | EventBody::AccountUsage(_)
            )
        {
            w.progress_at = Some(ev.ts);
        }
        match &ev.body {
            // A turn's start and end (control events, never pruned) stand for the progress
            // before them: what an older turn's agent events said is not kept, so a fold after
            // they were pruned (`hub::prune`) is the one before.
            EventBody::TurnStarted(_) => {
                w.calls.clear();
                w.ended = None;
                w.turn_at = Some(ev.ts);
                w.progress_at = None;
            }
            EventBody::TurnEnded(_) => {
                w.calls.clear();
                w.ended = None;
                w.progress_at = Some(ev.ts);
            }
            EventBody::ThreadStarted(_)
            | EventBody::ThreadStopped(_)
            | EventBody::ThreadCrashed(_)
            | EventBody::ThreadParked(_)
            | EventBody::ThreadAttached(_) => {
                w.calls.clear();
            }
            EventBody::AgentToolCall(c) => {
                w.calls.retain(|o| o.id != c.call_id);
                let what = twr_str(c.ext.as_ref(), PURPOSE_KEY)
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .unwrap_or(c.input_summary.as_str());
                w.calls.push(OpenCall {
                    id: c.call_id.clone(),
                    tool: c.tool.clone(),
                    what: twr_core::activity::cut(what, twr_core::activity::PHRASE_MAX),
                    since: ev.ts,
                    waits_for: twr_str(c.ext.as_ref(), WAITS_FOR_KEY)
                        .filter(|t| !t.is_empty())
                        .map(str::to_string),
                });
                if w.calls.len() > CALLS_KEPT {
                    w.calls.remove(0);
                }
            }
            EventBody::AgentToolResult(r) => {
                w.calls.retain(|o| o.id != r.call_id);
            }
            EventBody::TaskUpdated(p) if p.running => {
                if let Some((what, background)) = view_task {
                    let progress_at = match w.tasks.get(&p.task_id) {
                        Some(t) if p.heartbeat => t.progress_at,
                        _ => ev.ts,
                    };
                    w.tasks.insert(
                        p.task_id.clone(),
                        TaskFold {
                            progress_at,
                            what,
                            background,
                        },
                    );
                    if w.tasks.len() > TASKS_KEPT {
                        // The one that progressed longest ago goes.
                        if let Some(oldest) = w
                            .tasks
                            .iter()
                            .min_by_key(|(_, t)| t.progress_at)
                            .map(|(id, _)| id.clone())
                        {
                            w.tasks.remove(&oldest);
                        }
                    }
                }
            }
            EventBody::TaskUpdated(p) => {
                if let Some(t) = w.tasks.remove(&p.task_id) {
                    if t.background && open.is_some() {
                        w.ended = Some(Ended {
                            id: p.task_id.clone(),
                            what: t.what,
                            at: ev.ts,
                        });
                    }
                }
            }
            _ => {}
        }
        // A runtime's tasks end with its process (the view dropped them).
        if matches!(
            ev.body,
            EventBody::ThreadStarted(_)
                | EventBody::ThreadStopped(_)
                | EventBody::ThreadCrashed(_)
                | EventBody::ThreadParked(_)
                | EventBody::ThreadAttached(_)
        ) {
            if let Some(v) = self.registry.get(&ev.thread) {
                let w = &mut self.activity.entry(ev.thread.clone()).or_default().work;
                w.tasks.retain(|id, _| v.background_tasks.contains_key(id));
            }
        }
    }

    /// Since when thread `v`'s open turn is quiet: its runtime's last progress, or the turn's
    /// start when later ([`Work::quiet_secs`] counts from it); `None` before either is known.
    pub(super) fn quiet_since(&self, v: &ThreadView) -> Option<DateTime<Utc>> {
        let f = &self.activity.get(&v.id)?.work;
        match (f.progress_at, f.turn_at) {
            (Some(p), Some(t)) => Some(p.max(t)),
            (p, t) => p.or(t),
        }
    }

    /// What thread `v` has in flight at `now` (see the module docs).
    pub(super) fn work(&self, v: &ThreadView, now: DateTime<Utc>) -> Work {
        let own = super::activity::own_work(v);
        let v = own.as_ref();
        let fold = self.activity.get(&v.id).map(|a| &a.work);
        let mut w = Work::new(now);
        w.sight = if !self.box_online(&v.box_name) {
            Sight::Stale
        } else if v.attached.is_some() || v.runtime.starts_with("pty") {
            Sight::None
        } else if matches!(v.runtime.as_str(), "claude-code" | "fake") {
            Sight::Full
        } else {
            Sight::Calls
        };
        w.turn = v.current_turn;
        if v.current_turn.is_some() {
            w.turn_started_at = fold.and_then(|f| f.turn_at);
            w.compacting_since = v.compacting;
            w.ended = fold.and_then(|f| f.ended.clone());
        }
        let open: Vec<&OpenCall> = match fold {
            Some(f) if v.current_turn.is_some() => f.calls.iter().collect(),
            _ => Vec::new(),
        };
        let calls: Vec<Call> = open
            .iter()
            .map(|c| Call {
                id: c.id.clone(),
                tool: c.tool.clone(),
                what: c.what.clone(),
                since: c.since,
                task: v
                    .background_tasks
                    .iter()
                    .find(|(_, t)| t.foreground_call.as_deref() == Some(c.id.as_str()))
                    .map(|(id, _)| id.clone())
                    .or_else(|| c.waits_for.clone()),
            })
            .collect();
        let mut tasks: Vec<Task> = v
            .background_tasks
            .iter()
            .map(|(id, t)| {
                let kind = if t.job {
                    TaskKind::Job
                } else if t.detached {
                    TaskKind::Detached
                } else if let Some(call) = &t.foreground_call {
                    // Its call returned (or the turn ended) while it runs: nothing waits for
                    // it any longer.
                    if open.iter().any(|c| &c.id == call) {
                        TaskKind::Foreground
                    } else {
                        TaskKind::Detached
                    }
                } else {
                    TaskKind::Background
                };
                let what = if t.job {
                    self.job_description(id)
                        .unwrap_or_else(|| t.description.clone())
                } else {
                    t.description.clone()
                };
                Task {
                    awaited: open
                        .iter()
                        .any(|c| c.waits_for.as_deref() == Some(id.as_str())),
                    id: id.clone(),
                    what: if what.is_empty() {
                        "background task".into()
                    } else {
                        what
                    },
                    kind,
                    since: t.started_at,
                    progress_at: fold
                        .and_then(|f| f.tasks.get(id))
                        .map(|f| f.progress_at)
                        .or(t.started_at),
                    alive_at: t.updated_at,
                }
            })
            .collect();
        tasks.sort_by_key(|t| (t.since.unwrap_or(t.alive_at), t.id.clone()));
        let mut calls = calls;
        w.more = calls.len().saturating_sub(work::LISTED_MAX)
            + tasks.len().saturating_sub(work::LISTED_MAX);
        // The newest calls, the tasks that run the longest... and every running piece of
        // background work first: what the reading stands on stays listed.
        if calls.len() > work::LISTED_MAX {
            calls.drain(..calls.len() - work::LISTED_MAX);
        }
        keep_listed(&mut tasks);
        w.calls = calls;
        w.tasks = tasks;
        w.progress_at = fold.and_then(|f| f.progress_at);
        w.alive_at = fold
            .and_then(|f| f.alive_at)
            .filter(|a| w.progress_at.is_none_or(|p| *a > p));
        if let Ok(count) = self.store.pending_count(&v.id) {
            if count > 0 {
                if let Ok(Some(head)) = self
                    .store
                    .pending_list(&v.id, 1)
                    .map(|l| l.into_iter().next())
                {
                    let from = match head.from {
                        TurnFrom::User => "user",
                        TurnFrom::Orchestrator => "orchestrator",
                        TurnFrom::Runtime if head.prompt.starts_with("[twr job ") => "job",
                        TurnFrom::Runtime => "hub",
                    };
                    w.queued = Some(Queued {
                        count: usize::try_from(count).unwrap_or(usize::MAX),
                        oldest_at: DateTime::parse_from_rfc3339(&head.queued_at)
                            .map_or(now, |d| d.with_timezone(&Utc)),
                        from: from.into(),
                    });
                }
            }
        }
        w.state = w.assess(v.is_blocked());
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cut to the most listed, the tasks the reading stands on stay: an awaited detached task
    /// behind eight older leftovers is kept, so the reading still sees the work it waits on.
    #[test]
    fn the_work_the_reading_stands_on_stays_listed() {
        let t0 = DateTime::parse_from_rfc3339("2026-10-28T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let task = |i: i64, kind: TaskKind, awaited: bool| Task {
            id: format!("t{i}"),
            what: "Wait for TUI tests".into(),
            kind,
            since: Some(t0 + chrono::Duration::seconds(i)),
            awaited,
            progress_at: None,
            alive_at: t0,
        };
        let mut tasks: Vec<Task> = (0..8).map(|i| task(i, TaskKind::Detached, false)).collect();
        tasks.push(task(8, TaskKind::Detached, true));
        tasks.push(task(9, TaskKind::Foreground, false));
        keep_listed(&mut tasks);
        assert_eq!(tasks.len(), work::LISTED_MAX);
        assert!(tasks.iter().any(|t| t.id == "t8" && t.awaited), "{tasks:?}");
        let mut w = Work::new(t0);
        w.tasks = tasks;
        assert_eq!(w.running_work().count(), 1);
        let since: Vec<_> = w.tasks.iter().map(|t| t.since).collect();
        let mut sorted = since.clone();
        sorted.sort();
        assert_eq!(since, sorted, "oldest first");
    }
}
