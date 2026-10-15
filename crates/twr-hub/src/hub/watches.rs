//! Watches: the orchestrator asks to be woken when a worker finishes.
//!
//! A successful worker report is a conversation update and starts no orchestrator turn
//! (`hub::updates`), so on its own the orchestrator never gets a turn to act on a standing
//! request such as "when api finishes, send its branch to the integrator". A watch is that
//! request, recorded: `orchestrator.watch {thread | "*", note, repeat}` (the `watch_thread` MCP
//! tool) appends a `conversation.update` on the orchestrator thread with `ext.twr.watch =
//! {id, thread, note, repeat}`, which the user sees and replay folds; `orchestrator.unwatch`
//! appends one with `ext.twr.unwatch = {ids}`. Watch ids are `w1`, `w2`, ... in log order.
//!
//! A watch fires on its worker's (or, for `"*"`, any worker's) next final report: a hub report
//! of a turn that completed with no background task left running. An interim report (the turn
//! yielded with tasks running, `report::interim_tasks`) never fires one; the turn's follow-up,
//! which the runtime may start itself after that work (`turn.started{from: runtime}`), does
//! with its own report. When the tasks end and no follow-up turn starts within
//! [`IDLE_GRACE`] (by log time), the watch fires on that instead: the worker went idle after an
//! interim report. A background task starting meanwhile cancels that line (the worker is busy
//! again); when it ends, a new line waits a full grace. A failed turn fires nothing, on its report or when its tasks end (its error
//! card already notifies, and an automatic retry may follow); neither do reports written by
//! the orchestrator or a user. A rewrite of an interim report stays interim while the turn's
//! tasks run (`Hub::write_report`), so the worker going idle still fires.
//!
//! Firing queues a notice line ([`super::orch`]) with the report, the watch id and its note;
//! a non-success report's own notice line carries them instead. A notice turn fires a watch
//! without `repeat` once, on its first line that carries it, and records the watches its
//! prompt fired (`ext.twr.watches_fired`): a watch without `repeat` is used up then, live and
//! on replay alike, and every line still queued stops carrying it (a line that was only there
//! for it goes). `unwatch` takes the watches off the queued lines the same way. A line the
//! notice turn left for the next one (past its line cap) or still held keeps its watches. A
//! watch whose line is dropped before delivery (the orchestrator stopped or parked) stays
//! armed. Watches go with their thread's deletion and with a conversation reset
//! (`orchestrator reset` starts a new conversation); an `orchestrator use` switch keeps them,
//! lists them in the handoff and carries the queued lines that fire them over to the new
//! orchestrator, which gets them after its handoff turn.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::ThreadView;

use super::orch::cut;
use super::{append_err, rpc, twr_ext, twr_ext_map, Caller, Core, Hub, Inner};
use crate::report;

/// `conversation.update` `ext.twr` key recording a new watch.
pub(super) const WATCH_KEY: &str = "watch";
/// `conversation.update` `ext.twr` key recording watches cancelled: `{ids}`.
pub(super) const UNWATCH_KEY: &str = "unwatch";
/// Notice `turn.started` `ext.twr` key: the watches whose lines the turn delivered.
pub(super) const WATCHES_FIRED_KEY: &str = "watches_fired";
/// Notice `turn.started` `ext.twr` key: the time its lines were chosen (RFC 3339); a held
/// line due after it stays queued.
pub(super) const NOTICE_CUTOFF_KEY: &str = "notice_cutoff";

/// Longest watch note, in bytes.
pub const WATCH_NOTE_MAX: usize = 500;
/// Most watches at once.
pub const WATCHES_MAX: usize = 64;
/// How long after a worker's background work ended with no turn open its watches wait for a
/// follow-up turn before they fire on the idle worker.
pub const IDLE_GRACE: chrono::Duration = chrono::Duration::seconds(5);

/// One watch.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Watch {
    pub(super) id: String,
    /// The worker's id; `None`: every worker, present and future.
    pub(super) thread: Option<String>,
    /// What the orchestrator wrote it should do when the watch fires.
    pub(super) note: String,
    /// Fires on every final report until cancelled; else once.
    pub(super) repeat: bool,
}

/// ` watch=<id> note="..."` for each of `watches`, as a notice line carries them.
pub(super) fn hit_text(watches: &[&Watch]) -> String {
    watches
        .iter()
        .map(|w| format!(" watch={} note={}", w.id, quoted(&w.note)))
        .collect()
}

/// Whether turn `turn` of worker `view` is its last one and completed: only such a turn's
/// report, or its worker going idle after it, fires watches.
pub(super) fn turn_completed(view: &ThreadView, turn: Option<u64>) -> bool {
    view.last_turn
        .as_ref()
        .is_some_and(|t| Some(t.turn) == turn && t.result == TurnResult::Completed)
}

fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

impl Core {
    /// The watches thread `thread` matches, oldest first.
    pub(super) fn matching_watches(&self, thread: &str) -> Vec<Watch> {
        self.orch
            .watches
            .iter()
            .filter(|w| w.thread.as_deref().is_none_or(|t| t == thread))
            .cloned()
            .collect()
    }

    /// Watch bookkeeping for a `conversation.update` on an orchestrator thread (live and on
    /// replay): a watch recorded or cancelled.
    pub(super) fn track_watch_update(&mut self, p: &ConversationUpdate) {
        let Some(twr) = twr_ext(&p.ext) else {
            return;
        };
        if let Some(w) = twr
            .get(WATCH_KEY)
            .and_then(|w| serde_json::from_value::<Watch>(w.clone()).ok())
        {
            self.orch.watch_count += 1;
            self.orch.watches.retain(|x| x.id != w.id);
            self.orch.watches.push(w);
        }
        if let Some(ids) = twr
            .get(UNWATCH_KEY)
            .and_then(|u| u.get("ids"))
            .and_then(Value::as_array)
        {
            let ids: Vec<String> = ids
                .iter()
                .filter_map(|id| id.as_str().map(str::to_string))
                .collect();
            self.orch.watches.retain(|w| !ids.contains(&w.id));
            self.orch.drop_watch_hits(&ids);
        }
    }

    /// A notice turn delivered `fired` (its `ext.twr.watches_fired`): those without `repeat`
    /// are used up, and no queued line fires them again.
    pub(super) fn consume_watches(&mut self, fired: &[Value]) {
        let used: Vec<String> = self
            .orch
            .watches
            .iter()
            .filter(|w| !w.repeat && fired.iter().any(|id| id.as_str() == Some(&w.id)))
            .map(|w| w.id.clone())
            .collect();
        self.orch.watches.retain(|w| !used.contains(&w.id));
        self.orch.drop_watch_hits(&used);
    }

    /// Thread `id` was deleted: its own watches go (a `"*"` watch stays).
    pub(super) fn drop_thread_watches(&mut self, id: &str) {
        self.orch
            .watches
            .retain(|w| w.thread.as_deref() != Some(id));
        self.orch.idle_fired.remove(id);
    }

    /// A worker's task ended (`ev`, a `task.updated` not running): when that left it with no
    /// turn and no task after an interim report of a turn that completed, its watches get a
    /// held notice line, due [`IDLE_GRACE`] later and dropped if the worker starts a turn first
    /// (its follow-up's report fires them then). Once per interim report.
    pub(super) fn queue_idle_watches(&mut self, ev: &Event) {
        if ev.thread == HUB_THREAD || self.is_orch(&ev.thread) || !self.orch_active() {
            return;
        }
        let Some(v) = self
            .registry
            .get(&ev.thread)
            .filter(|v| !v.is_orchestrator() && !v.has_work() && v.reported)
        else {
            return;
        };
        let Some(r) = v
            .last_report
            .as_ref()
            .filter(|r| report::interim_tasks(r) > 0)
        else {
            return;
        };
        let turn = r.turn.or(v.last_turn.as_ref().map(|t| t.turn));
        if !turn_completed(v, turn) || self.orch.idle_fired.get(&ev.thread) == Some(&turn) {
            return;
        }
        let hits = self.matching_watches(&ev.thread);
        if hits.is_empty() {
            return;
        }
        let line = format!(
            "- thread={} idle: its background work finished and no follow-up turn started; \
             its last report (turn {}) was interim: summary={} thread_id={}",
            v.name,
            turn.map_or("?".into(), |t| t.to_string()),
            quoted(&cut(&r.summary, super::orch::NOTICE_SUMMARY_MAX)),
            ev.thread
        );
        self.orch.idle_fired.insert(ev.thread.clone(), turn);
        self.orch
            .push_held(&ev.thread, &ev.id, line, hits, ev.ts + IDLE_GRACE);
    }

    /// Worker `ev.thread` runs background task `task` again (`ev`, a `task.updated` running
    /// that its view took): it is not idle after all. Its held idle line goes, overdue or not,
    /// and the guard with it, so the worker going idle again holds a new line for a full
    /// [`IDLE_GRACE`].
    pub(super) fn cancel_idle_watches(&mut self, ev: &Event, task: &str) {
        if !self
            .registry
            .get(&ev.thread)
            .is_some_and(|v| v.background_tasks.contains_key(task))
        {
            return;
        }
        if self.orch.drop_idle(&ev.thread) {
            self.orch.idle_fired.remove(&ev.thread);
        }
    }

    /// The active watches as the tools show them.
    pub(super) fn watches_json(&self) -> Vec<Value> {
        self.orch
            .watches
            .iter()
            .map(|w| {
                json!({
                    "watch": w.id,
                    "thread": w.thread.as_deref().unwrap_or("*"),
                    "thread_name": match &w.thread {
                        Some(t) => self.registry.get(t).map(|v| v.name.clone()),
                        None => Some("every worker".into()),
                    },
                    "note": w.note,
                    "repeat": w.repeat,
                })
            })
            .collect()
    }

    /// The watches, one line each, for a handoff (empty: none).
    pub(super) fn watch_lines(&self) -> Vec<String> {
        self.orch
            .watches
            .iter()
            .map(|w| {
                let who = match &w.thread {
                    Some(t) => self
                        .registry
                        .get(t)
                        .map_or_else(|| t.clone(), |v| format!("{} ({t})", v.name)),
                    None => "every worker".into(),
                };
                format!(
                    "- {} on {who}{}: {}",
                    w.id,
                    if w.repeat { ", repeating" } else { "" },
                    quoted(&w.note)
                )
            })
            .collect()
    }

    fn watch_locked(
        &mut self,
        hub: &Arc<Inner>,
        query: &str,
        note: &str,
        repeat: bool,
    ) -> Result<Value, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let note = note.trim();
        if note.is_empty() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "note is empty: say what to do when it fires",
            ));
        }
        if note.len() > WATCH_NOTE_MAX {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("note is longer than {WATCH_NOTE_MAX} bytes"),
            ));
        }
        let Some(orch) = self.orch.thread.clone() else {
            return Err(rpc(codes::CONFLICT, "there is no orchestrator to wake"));
        };
        let thread = match query.trim() {
            "*" => None,
            q => {
                let id = self.resolve(q)?;
                let v = self.view(&id)?;
                if v.is_orchestrator() || id == HUB_THREAD {
                    return Err(rpc(
                        codes::BAD_REQUEST,
                        format!("{} is no worker thread", v.name),
                    ));
                }
                Some(id)
            }
        };
        let view = thread
            .as_deref()
            .and_then(|t| self.registry.get(t))
            .cloned();
        let mut out = match &view {
            Some(v) => json!({
                "thread": v.id,
                "thread_name": v.name,
                "status": v.status(),
                "background_task_count": v.background_tasks.len(),
                "last_report": v.last_report.as_ref().map(|r| json!({
                    "turn": r.turn,
                    "grade": twr_decisions::grade_str(r.grade),
                    "summary": cut(&r.summary, super::orch::NOTICE_SUMMARY_MAX),
                    "interim": report::interim_tasks(r) > 0,
                    "follow_up": report::is_follow_up(r),
                })),
            }),
            None => json!({ "thread": "*", "thread_name": "every worker" }),
        };
        if let Some(w) = self
            .orch
            .watches
            .iter()
            .find(|w| w.thread == thread && w.note == note && w.repeat == repeat)
        {
            out["watch"] = json!(w.id);
            out["existing"] = json!(true);
            out["watches"] = json!(self.watches_json());
            return Ok(out);
        }
        if self.orch.watches.len() >= WATCHES_MAX {
            return Err(rpc(
                codes::CONFLICT,
                format!("{WATCHES_MAX} watches are set already: unwatch some first"),
            ));
        }
        let watch = Watch {
            id: format!("w{}", self.orch.watch_count + 1),
            thread: thread.clone(),
            note: note.to_string(),
            repeat,
        };
        let who = view.as_ref().map_or("every worker", |v| v.name.as_str());
        let text = format!(
            "Watching {who}: the orchestrator is woken {} it finishes ({}). Note: {note}",
            if repeat { "each time" } else { "the next time" },
            watch.id
        );
        let mut twr = serde_json::Map::new();
        twr.insert(
            WATCH_KEY.into(),
            serde_json::to_value(&watch).map_err(super::internal)?,
        );
        self.append(
            hub,
            &orch,
            None,
            Source::Hub,
            EventBody::ConversationUpdate(ConversationUpdate {
                text,
                about_thread: thread,
                ext: twr_ext_map(Value::Object(twr)),
                ..Default::default()
            }),
        )
        .map_err(append_err)?;
        out["watch"] = json!(watch.id);
        out["existing"] = json!(false);
        out["watches"] = json!(self.watches_json());
        Ok(out)
    }

    fn unwatch_locked(&mut self, hub: &Arc<Inner>, which: &str) -> Result<Value, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let which = which.trim();
        let gone: Vec<Watch> = match which {
            "all" | "*" => self.orch.watches.clone(),
            id => match self.orch.watches.iter().find(|w| w.id == id) {
                Some(w) => vec![w.clone()],
                None => {
                    return Err(rpc(
                        codes::NOT_FOUND,
                        format!("no watch {id} (list_watches lists them)"),
                    ))
                }
            },
        };
        if let (false, Some(orch)) = (gone.is_empty(), self.orch.thread.clone()) {
            let names: Vec<String> = gone
                .iter()
                .map(|w| match &w.thread {
                    Some(t) => self
                        .registry
                        .get(t)
                        .map_or_else(|| t.clone(), |v| v.name.clone()),
                    None => "every worker".into(),
                })
                .collect();
            let ids: Vec<&str> = gone.iter().map(|w| w.id.as_str()).collect();
            self.append(
                hub,
                &orch,
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: format!(
                        "Stopped watching {} ({}).",
                        names.join(", "),
                        ids.join(", ")
                    ),
                    about_thread: match &gone[..] {
                        [w] => w.thread.clone(),
                        _ => None,
                    },
                    ext: twr_ext_map(json!({ UNWATCH_KEY: { "ids": ids } })),
                    ..Default::default()
                }),
            )
            .map_err(append_err)?;
        }
        Ok(json!({
            "removed": gone.iter().map(|w| w.id.clone()).collect::<Vec<_>>(),
            "watches": self.watches_json(),
        }))
    }
}

impl Hub {
    /// `orchestrator.watch`: wake the orchestrator when worker `thread` (or `"*"`: every
    /// worker) ends a turn with a final report, with `note` (see the module docs). The same
    /// watch asked again returns the existing one.
    pub fn watch_thread(
        &self,
        thread: &str,
        note: &str,
        repeat: bool,
        _caller: &Caller,
    ) -> Result<Value, RpcError> {
        self.inner
            .core()
            .watch_locked(&self.inner, thread, note, repeat)
    }

    /// `orchestrator.unwatch`: cancels watch `which` (an id, or `"all"`).
    pub fn unwatch(&self, which: &str, _caller: &Caller) -> Result<Value, RpcError> {
        self.inner.core().unwatch_locked(&self.inner, which)
    }

    /// `orchestrator.watches`: the active watches.
    pub fn watches(&self) -> Value {
        json!({ "watches": self.inner.core().watches_json() })
    }
}

/// The cutoff a notice turn recorded (`ext.twr.notice_cutoff`), else `fallback` (its time).
pub(super) fn notice_cutoff(p: &TurnStarted, fallback: DateTime<Utc>) -> DateTime<Utc> {
    twr_ext(&p.ext)
        .and_then(|t| t.get(NOTICE_CUTOFF_KEY))
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map_or(fallback, |d| d.with_timezone(&Utc))
}
