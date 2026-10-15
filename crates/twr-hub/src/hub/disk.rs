//! Disk: what each thread's worktree takes, how much its box has free, the disk card and
//! trimming (`twr trim`).
//!
//! **Measuring.** Every [`DISK_TICK`] (5 minutes) the hub asks each box that is online how much
//! it has free where its worktrees are ([`GitOps::disk_usage`], each within [`FREE_TIMEOUT`]),
//! then has it measure the hub-managed worktrees that are due (for at most [`WALKS_BUDGET`] a
//! round, the rest the next one): never measured, measured more than
//! [`REMEASURE_ACTIVE`] ago with a turn or a trim since, or more than [`REMEASURE_IDLE`] ago.
//! One walk at a time, so the measuring itself never loads the machine much. A measurement is
//! recorded as `worktree.disk` on the thread only when it moved by more than [`CHANGE_MIN`] or
//! 5% (the thread's view keeps the latest, less what trims freed since). The free space is kept
//! in memory per box ([`DiskState::boxes`], `hub.info`'s `disk`).
//!
//! **The disk card.** When a box's free space falls under `[threads] disk_warn_gb`, the hub
//! raises a question on [`HUB_THREAD`] (`ext.twr.disk` = `{box, free, threshold, threads:
//! [{thread, name, build_bytes}]}`, options [`TRIM`] and [`NOT_NOW`]): how much is left, and
//! the idle threads on that box whose worktrees hold build output, the most first. Only the
//! user answers it. Trim trims those of them still idle, one after the other; Not now (or a
//! dismissal) closes it. Either way the box gets no new card until its free space is back over
//! the threshold, or the hub restarts. An open card closes by itself once the free space is
//! back over the threshold.
//!
//! **Trimming** (`thread.trim {thread}`, the user only, on the hub's own box for now: a runner's
//! answer that times out would leave the outcome unknown while the thread goes on): the box deletes the worktree's build
//! output the way a deletion deletes it ([`GitOps::worktree_trim`]), while the thread takes no
//! turn: refused while a turn or background work runs, while it is attached, parked or
//! resumed, deleted or trimmed already. A live idle runtime stays (its session is kept); the
//! thread's prompts wait until the trim is over. `worktree.trimmed` records what went, by
//! whoever asked.
//!
//! **Idle trims.** A project with `[projects.<name>] trim_idle_after` has the build output of
//! its threads trimmed by the disk round, by the hub, once they have been idle that long
//! ([`ThreadView::worked_at`](twr_core::reduce::ThreadView::worked_at)): its configured
//! threads on the hub's own box (Linux and macOS) whose hub-managed worktree holds build output
//! as last measured, not trimmed since they last worked, and trimmable as for `thread.trim`.
//! One at a time, after the round's measurements. Of the build output, an idle trim deletes
//! only what `idle_trim_disposable` names too ([`TrimRequest::only`],
//! [`Config::idle_trim_disposable`](twr_core::config::Config::idle_trim_disposable)): what a
//! build always makes again (`target/`, `node_modules/`, ...), not `dist/` or `.venv/`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::git::{human_bytes, DiskReading, GitOps, TrimRequest, WorktreeDisk};
use twr_core::proto::{codes, RpcError};

use super::{
    rpc, twr_ext, twr_ext_map, AdapterScope, Batch, Caller, Core, Hub, HubState, Inner, ThreadRt,
};

/// How often the hub reads its boxes' free space and measures the worktrees that are due.
pub const DISK_TICK: Duration = Duration::from_secs(300);
/// A worktree that had a turn or a trim since its last measurement is measured again once
/// that measurement is this old.
pub const REMEASURE_ACTIVE: Duration = Duration::from_secs(600);
/// Any worktree is measured again once its last measurement is this old.
pub const REMEASURE_IDLE: Duration = Duration::from_secs(6 * 3600);
/// A measurement is recorded when it moved by more than this many bytes or 5%.
pub const CHANGE_MIN: u64 = 50 << 20;
/// Build output under this many bytes is not worth naming on a disk card.
pub const CARD_MIN_BUILD: u64 = 100 << 20;
/// How long reading a box's free space may take in a round.
pub const FREE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a round goes on starting worktree walks; the ones left wait for the next round.
pub const WALKS_BUDGET: Duration = Duration::from_secs(120);
/// Most threads a disk card names (the biggest).
pub const CARD_THREADS_MAX: usize = 32;
/// `ext.twr` key of a disk card.
pub const DISK_KEY: &str = "disk";
/// The disk card's option that trims the threads it names.
pub const TRIM: &str = "Trim";
/// The disk card's option that closes it.
pub const NOT_NOW: &str = "Not now";
/// Whether the hub's own box trims: only where it can list the mount points in a worktree.
const TRIMS_HERE: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// The hub's disk bookkeeping (memory only; the cards come back from the log).
#[derive(Default)]
pub(super) struct DiskState {
    /// When each thread's worktree was last measured, and its turn count and last trim then.
    measured: HashMap<String, Measured>,
    /// Each box's free space, as last read.
    pub(super) boxes: HashMap<String, BoxDisk>,
    /// The open disk cards, by attention id.
    pub(super) cards: HashMap<String, DiskCard>,
    /// Boxes whose card the user answered: no new one until their free space is back over the
    /// threshold.
    quiet: HashSet<String>,
}

struct Measured {
    /// When the measurement was asked for.
    at: Instant,
    /// The same, as the log's clock (compared with the thread's `last_activity`).
    at_wall: DateTime<Utc>,
}

/// A box's free space, as last read.
#[derive(Debug, Clone)]
pub(super) struct BoxDisk {
    pub(super) free: Option<u64>,
    pub(super) total: Option<u64>,
    pub(super) at: DateTime<Utc>,
}

/// An open disk card: its box and the threads Trim trims, each with the life of it the card
/// named (`lifecycle_seq`: one deleted and restored since is another life, and is left alone).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DiskCard {
    pub(super) box_name: String,
    pub(super) threads: Vec<(String, u64)>,
}

impl DiskCard {
    /// The card a disk card's `attention.raised` describes (`ext.twr.disk`).
    pub(super) fn from_raised(p: &AttentionRaised) -> Option<DiskCard> {
        let d = twr_ext(&p.ext)?.get(DISK_KEY)?;
        Some(DiskCard {
            box_name: d.get("box")?.as_str()?.to_string(),
            threads: d
                .get("threads")?
                .as_array()?
                .iter()
                .filter_map(|t| {
                    Some((
                        t.get("thread")?.as_str()?.to_string(),
                        t.get("lifecycle")?.as_u64()?,
                    ))
                })
                .collect(),
        })
    }
}

/// Whether an `attention.raised` on [`HUB_THREAD`] is a disk card.
pub(super) fn is_disk_card(ext: &Option<JsonMap>) -> bool {
    twr_ext(ext).is_some_and(|t| t.contains_key(DISK_KEY))
}

/// Whether `new` differs enough from `old` to be recorded.
fn moved(old: Option<&WorktreeDisk>, new: &WorktreeDisk) -> bool {
    let Some(old) = old else {
        return true;
    };
    let far = |a: u64, b: u64| a.abs_diff(b) > CHANGE_MIN.max(a.max(b) / 20);
    // Build output coming or going counts whatever its size: an idle trim needs to know.
    old.partial != new.partial
        || far(old.bytes, new.bytes)
        || far(old.build_bytes, new.build_bytes)
        || (old.build_bytes == 0) != (new.build_bytes == 0)
}

impl Core {
    /// `hub.info`'s `disk`: each box's free space as last read, and whether it is under
    /// `[threads] disk_warn_gb`: `{box: {free, total, at, low}}`.
    pub(super) fn disk_json(&self, hub: &Inner) -> Value {
        let threshold = hub.config.threads.disk_warn_gb.saturating_mul(1 << 30);
        Value::Object(
            self.disk
                .boxes
                .iter()
                .map(|(b, d)| {
                    let low = threshold > 0 && d.free.is_some_and(|f| f < threshold);
                    (
                        b.clone(),
                        json!({"free": d.free, "total": d.total, "at": d.at, "low": low}),
                    )
                })
                .collect(),
        )
    }

    /// Whether `thread` is idle enough to trim: no turn or background work, not attached
    /// (nor being), parked or resumed, deleted, trimmed, or given up on. `Err` says why not.
    fn trimmable(&self, id: &str) -> Result<(), String> {
        let view = self.registry.get(id).ok_or("it is gone")?;
        let name = &view.name;
        if let Some(turn) = view.current_turn {
            return Err(format!(
                "turn {turn} of {name} is running; trim it once it is idle"
            ));
        }
        if !view.background_tasks.is_empty() {
            return Err(format!(
                "{name} has background work running; trim it once that ends"
            ));
        }
        if let Some(c) = &view.attached {
            return Err(format!("{name} is attached by {c}"));
        }
        let rt = self.rt.get(id);
        if rt.is_some_and(|r| r.attach.is_some()) {
            return Err(format!("{name} is being attached"));
        }
        if rt.is_some_and(|r| r.park.is_some() || r.resuming) {
            return Err(format!("{name} is being parked or resumed"));
        }
        if rt.is_some_and(|r| r.deleting) {
            return Err(format!("{name} is being deleted"));
        }
        if rt.is_some_and(|r| r.trimming) {
            return Err(format!("{name} is being trimmed already"));
        }
        if rt.is_some_and(ThreadRt::giving_up) {
            return Err(format!(
                "{name}'s runtime is exiting; try again in a moment"
            ));
        }
        if rt.is_some_and(|r| r.restart_reason.is_some() || r.stop_requested || r.exiting) {
            return Err(format!(
                "{name}'s runtime is being stopped or restarted; try again in a moment"
            ));
        }
        // An item its runtime raised between turns waits on that process: stopping it would
        // resolve the item unanswered.
        if !self
            .open_adapter_attention(id, AdapterScope::All, false)
            .is_empty()
        {
            return Err(format!("{name} is waiting on you; answer it first"));
        }
        Ok(())
    }

    /// Starts trimming `id`: checks it ([`Core::trimmable`], a hub-managed worktree on a box
    /// that is online) and marks it trimming, so it takes no turn. An idle runtime up in the
    /// worktree is stopped first, its session and queue kept (`thread.stopped` with reason
    /// [`TRIM_RESTART_REASON`]): nothing may start work there while its build output goes,
    /// and its box refuses the trim while a runtime works in it. What its box is asked, and
    /// whether a runtime is being stopped.
    fn begin_trim(
        &mut self,
        hub: &Inner,
        id: &str,
    ) -> Result<(Arc<dyn GitOps>, TrimRequest, String, bool), RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let view = self.view(id)?.clone();
        let name = view.name.clone();
        if view.is_orchestrator() {
            return Err(rpc(codes::BAD_REQUEST, "the orchestrator has no worktree"));
        }
        self.require_box(&view)?;
        let Some((box_name, worktree)) = self
            .managed_worktree(id)
            .map(|(b, w)| (b.to_string(), w.to_path_buf()))
        else {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{name} works in a directory twr did not make; twr trims only its own \
                     worktrees"
                ),
            ));
        };
        if !TRIMS_HERE {
            return Err(rpc(
                codes::CONFLICT,
                "twr trims build output only where it can list the mount points (Linux and \
                 macOS)",
            ));
        }
        if box_name != "local" {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{name} runs on box {box_name}: twr trims only on the hub's own box for now"
                ),
            ));
        }
        let Some(repo) = view.repo.clone().map(PathBuf::from) else {
            return Err(rpc(codes::CONFLICT, format!("{name} has no repository")));
        };
        self.trimmable(id)
            .map_err(|why| rpc(codes::CONFLICT, why))?;
        if let Some((other, client)) = self.attach_inside(&box_name, &worktree, id) {
            return Err(rpc(
                codes::CONFLICT,
                format!("{other} is attached by {client} inside {name}'s worktree"),
            ));
        }
        let git = hub
            .hosts
            .get(&box_name)
            .map(|h| h.git())
            .ok_or_else(|| rpc(codes::CONFLICT, format!("box {box_name} is not configured")))?;
        let stopping = self.rt.get(id).is_some_and(|r| r.live.is_some());
        if stopping {
            // As a settings restart stops it (`hub::restart`): recorded first, the queue kept.
            let remote_stop = self.remote_stop(hub, id);
            let batch = Batch {
                stop_requests: vec![id.to_string()],
                remote_cmds: remote_stop.iter().cloned().collect(),
                ..Batch::default()
            };
            self.append_batch(hub, batch).map_err(super::append_err)?;
            if let Some(c) = &remote_stop {
                if let Some(rbox) = hub.remotes.get(&c.box_name) {
                    rbox.expect_stop(id, &c.cmd_id);
                }
            }
            let rt = self.rt(id);
            rt.stop_requested = true;
            rt.restart_reason = Some(TRIM_RESTART_REASON);
            if let Some(live) = rt.live.as_mut() {
                live.stop_requested = true;
                let _ = live.handle.stop();
            }
        }
        self.rt(id).trimming = true;
        // What tells the thread's own worktree from another thread's checkout moved there, as a
        // deletion checks it.
        let branch = match self.rt.get(id).map(|r| &r.workspace) {
            Some(super::git::Workspace::Worktree { branch, .. }) => Some(branch.clone()),
            _ => None,
        };
        let other_branches = Some(self.other_branches(id, &box_name));
        Ok((
            git,
            TrimRequest {
                repo,
                worktree,
                branch,
                other_branches,
                only: None,
            },
            name,
            stopping,
        ))
    }

    /// Ends `id`'s trim: records what went (by `source`) when something went, and runs what
    /// queued meanwhile. An error when what went could not be recorded.
    fn end_trim(
        &mut self,
        hub: &Arc<Inner>,
        id: &str,
        source: Source,
        outcome: Option<&twr_core::git::TrimOutcome>,
    ) -> anyhow::Result<()> {
        let Some(rt) = self.rt.get_mut(id) else {
            return Ok(());
        };
        rt.trimming = false;
        let mut recorded = Ok(());
        if let Some(o) = outcome.filter(|_| self.registry.get(id).is_some()) {
            let body = EventBody::WorktreeTrimmed(WorktreeTrimmed {
                deleted: o.deleted.clone(),
                freed: o.freed,
                notes: o.notes.clone(),
                ..Default::default()
            });
            recorded = self.append(hub, id, None, source, body).map(|_| ());
        }
        if let Err(e) = self.try_dispatch(hub, id) {
            tracing::warn!(thread = id, error = %format!("{e:#}"), "dispatch after a trim failed");
        }
        recorded
    }

    /// The threads a disk card for `box_name` names: idle ones there whose worktrees hold at
    /// least [`CARD_MIN_BUILD`] of build output, the most first, at most [`CARD_THREADS_MAX`].
    fn trim_candidates(&self, box_name: &str) -> Vec<Candidate> {
        // Only where a box can trim: the hub's own box, when it is Linux or macOS (a remote
        // box says so itself when it cannot).
        if box_name != "local" || !TRIMS_HERE {
            return Vec::new();
        }
        let mut out: Vec<Candidate> = self
            .registry
            .list()
            .iter()
            .filter(|v| v.box_name == box_name && !v.is_orchestrator() && !v.retired)
            .filter(|v| {
                self.managed_worktree(&v.id)
                    .is_some_and(|(b, wt)| self.attach_inside(b, wt, &v.id).is_none())
            })
            .filter_map(|v| {
                let build = v.disk.as_ref()?.build_bytes;
                (build >= CARD_MIN_BUILD && self.trimmable(&v.id).is_ok()).then(|| Candidate {
                    id: v.id.clone(),
                    name: v.name.clone(),
                    build,
                    lifecycle: v.lifecycle_seq,
                })
            })
            .collect();
        out.sort_by(|a, b| b.build.cmp(&a.build).then(a.name.cmp(&b.name)));
        out.truncate(CARD_THREADS_MAX);
        out
    }

    /// The threads to trim for being idle at `now` ([module docs](self), "Idle trims"), each
    /// with its life (`lifecycle_seq`).
    fn idle_trims(&self, hub: &Inner, now: DateTime<Utc>) -> Vec<(String, u64)> {
        if !TRIMS_HERE {
            return Vec::new();
        }
        self.registry
            .list()
            .iter()
            .filter(|v| self.idle_trim_due(hub, &v.id, now))
            .map(|v| (v.id.clone(), v.lifecycle_seq))
            .collect()
    }

    /// Whether thread `id` is due an idle trim at `now` ([module docs](self), "Idle trims").
    fn idle_trim_due(&self, hub: &Inner, id: &str, now: DateTime<Utc>) -> bool {
        let Some(v) = self.registry.get(id) else {
            return false;
        };
        if v.box_name != "local" || v.is_orchestrator() || v.retired || !v.configured_project {
            return false;
        }
        let Some(after) = v
            .project
            .as_deref()
            .and_then(|p| hub.config.projects.get(p))
            .and_then(|p| p.trim_idle_after())
        else {
            return false;
        };
        // A project whose idle trims may delete nothing has none (its runtime is left alone).
        if hub
            .config
            .idle_trim_disposable(v.project.as_deref())
            .is_empty()
        {
            return false;
        }
        let Some(since) = v.worked_at.or(v.last_activity).or(v.created_at) else {
            return false;
        };
        let idle = now.signed_duration_since(since).to_std().ok();
        if idle.is_none_or(|i| i < after)
            || v.trimmed.as_ref().is_some_and(|(at, _)| *at >= since)
            || v.disk.as_ref().is_none_or(|d| d.build_bytes == 0)
        {
            return false;
        }
        self.managed_worktree(id)
            .is_some_and(|(b, wt)| self.attach_inside(b, wt, id).is_none())
            && self.trimmable(id).is_ok()
    }

    /// Notes box `box_name`'s free space `reading`, and raises or closes its disk card.
    /// With `raise` false it only notes the reading and closes a card the free space is back
    /// over the threshold for ([`Core::raise_if_low`] raises one later).
    fn note_box_disk(&mut self, hub: &Inner, box_name: &str, reading: &DiskReading, raise: bool) {
        let threshold = hub.config.threads.disk_warn_gb.saturating_mul(1 << 30);
        self.disk.boxes.insert(
            box_name.to_string(),
            BoxDisk {
                free: reading.free,
                total: reading.total,
                at: Utc::now(),
            },
        );
        let Some(free) = reading.free else {
            return;
        };
        let open: Vec<String> = self
            .disk
            .cards
            .iter()
            .filter(|(_, c)| c.box_name == box_name)
            .map(|(a, _)| a.clone())
            .collect();
        if threshold == 0 || free >= threshold {
            self.disk.quiet.remove(box_name);
            for attn in open {
                let body = EventBody::AttentionResolved(AttentionResolved {
                    attn_id: attn.clone(),
                    by: ResolvedBy::Timeout,
                    answer: None,
                    ..Default::default()
                });
                if let Err(e) = self.append(hub, HUB_THREAD, None, Source::Hub, body) {
                    tracing::warn!(error = %format!("{e:#}"), "cannot close a disk card");
                }
            }
            return;
        }
        if !raise || !open.is_empty() || self.disk.quiet.contains(box_name) {
            return;
        }
        let threads = self.trim_candidates(box_name);
        let mut summary = disk_card_summary(box_name, free, threshold, &threads);
        let cannot = if box_name != "local" {
            Some("twr trims only on the hub's own box for now")
        } else if !TRIMS_HERE {
            Some("twr trims build output only on Linux and macOS")
        } else {
            None
        };
        if let Some(why) = cannot {
            summary = format!(
                "Disk low on {box_name}: {} free, under {}. {why}; `twr thread list` shows each \
                 worktree's size.",
                human_bytes(free),
                human_bytes(threshold)
            );
        }
        let options = if threads.is_empty() {
            vec![NOT_NOW.to_string()]
        } else {
            vec![TRIM.to_string(), NOT_NOW.to_string()]
        };
        let ext = json!({ DISK_KEY: {
            "box": box_name,
            "free": free,
            "threshold": threshold,
            "threads": threads.iter().map(|c| {
                json!({"thread": c.id, "name": c.name, "build_bytes": c.build,
                       "lifecycle": c.lifecycle})
            }).collect::<Vec<_>>(),
        }});
        let body = EventBody::AttentionRaised(AttentionRaised {
            attn_id: new_id(),
            reason: AttentionReason::Question,
            summary,
            options,
            ext: twr_ext_map(ext),
            ..Default::default()
        });
        if let Err(e) = self.append(hub, HUB_THREAD, None, Source::Hub, body) {
            tracing::warn!(error = %format!("{e:#}"), "cannot raise a disk card");
        }
    }

    /// Raises box `box_name`'s disk card when its free space, as last read, is low
    /// ([`Core::note_box_disk`]).
    fn raise_if_low(&mut self, hub: &Inner, box_name: &str) {
        let Some(d) = self.disk.boxes.get(box_name) else {
            return;
        };
        let reading = DiskReading {
            free: d.free,
            total: d.total,
            worktree: None,
        };
        self.note_box_disk(hub, box_name, &reading, true);
    }

    /// Answers disk card `attn_id` (the user only): Trim trims the threads it names that are
    /// still idle, one after the other, in the background; anything else closes it. The box
    /// gets no new card until its free space is back over the threshold.
    pub(super) fn answer_disk_card(
        &mut self,
        hub: &Arc<Inner>,
        attn_id: &str,
        card: &DiskCard,
        decision: Option<&str>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{attn_id} is a disk card: only the user answers it; tell the user it is \
                     waiting for them"
                ),
            ));
        }
        let trimming = decision == Some(TRIM);
        let answer = decision.map(Answer::new);
        let body = EventBody::AttentionResolved(AttentionResolved {
            attn_id: attn_id.to_string(),
            by: ResolvedBy::User,
            answer,
            ..Default::default()
        });
        let ev = self
            .append(hub, HUB_THREAD, None, caller.source(), body)
            .map_err(super::append_err)?;
        self.disk.quiet.insert(card.box_name.clone());
        if trimming {
            let (hub, card, source) = (Arc::clone(hub), card.clone(), caller.source());
            tokio::spawn(async move {
                let mut done = Vec::new();
                let mut not = Vec::new();
                for (t, life) in &card.threads {
                    let name = hub
                        .core()
                        .registry
                        .get(t)
                        .map_or_else(|| t.clone(), |v| v.name.clone());
                    match trim(&hub, t, Some(*life), source.clone(), false).await {
                        Ok(r) => done.push(format!(
                            "{name} ({})",
                            human_bytes(r["freed"].as_u64().unwrap_or(0))
                        )),
                        Err(e) => not.push(format!("{name} ({})", e.message)),
                    }
                }
                hub.core()
                    .report_card_trims(&hub, &card.box_name, &done, &not);
            });
        }
        Ok(ev)
    }

    /// Says what a disk card's Trim did: in the orchestrator's conversation, or, without an
    /// orchestrator, on an error card when some thread was not trimmed.
    fn report_card_trims(&mut self, hub: &Inner, box_name: &str, done: &[String], not: &[String]) {
        let mut text = format!("Disk card for {box_name}:");
        if !done.is_empty() {
            text.push_str(&format!(" trimmed {}.", done.join(", ")));
        }
        if !not.is_empty() {
            text.push_str(&format!(" Not trimmed: {}.", not.join("; ")));
        }
        let appended = match self.orch.thread.clone() {
            Some(orch) => self.append(
                hub,
                &orch,
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text,
                    ..Default::default()
                }),
            ),
            None if !not.is_empty() => self.append(
                hub,
                HUB_THREAD,
                None,
                Source::Hub,
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: new_id(),
                    reason: AttentionReason::Error,
                    summary: text,
                    options: vec!["dismiss".into()],
                    ..Default::default()
                }),
            ),
            None => return,
        };
        if let Err(e) = appended {
            tracing::warn!(error = %format!("{e:#}"), "cannot say what a disk card's Trim did");
        }
    }

    /// The hub-managed worktrees due for a measurement now, never measured ones first, then
    /// the longest unmeasured: a round that runs out of time leaves the most recently measured
    /// for later, so none waits forever.
    fn disk_due(&self, hub: &Inner) -> Vec<Due> {
        let now = Instant::now();
        let mut due: Vec<(Option<Instant>, Due)> = self
            .registry
            .list()
            .iter()
            .filter(|v| !v.is_orchestrator() && !v.retired)
            .filter(|v| self.box_online(&v.box_name))
            .filter(|v| !self.rt.get(&v.id).is_some_and(|r| r.deleting || r.trimming))
            .filter(|v| match self.disk.measured.get(&v.id) {
                None => true,
                Some(m) => {
                    let age = now.saturating_duration_since(m.at);
                    // Anything recorded since (a turn's events, a trim), or work running now.
                    let active = v.last_activity.is_some_and(|a| a > m.at_wall)
                        || v.current_turn.is_some()
                        || !v.background_tasks.is_empty();
                    age >= REMEASURE_IDLE || (active && age >= REMEASURE_ACTIVE)
                }
            })
            .filter_map(|v| {
                let (b, wt) = self.managed_worktree(&v.id)?;
                let git = hub.hosts.get(b)?.git();
                Some((
                    self.disk.measured.get(&v.id).map(|m| m.at),
                    Due {
                        id: v.id.clone(),
                        git,
                        worktree: wt.to_path_buf(),
                        trimmed: v.trimmed.as_ref().map(|t| t.0),
                        life: v.lifecycle_seq,
                    },
                ))
            })
            .collect();
        due.sort_by_key(|(at, _)| *at);
        due.into_iter().map(|(_, d)| d).collect()
    }

    /// Whether `d` still names the thread as it is (the same life, worktree and last trim):
    /// a thread deleted and restored, or trimmed, since is measured afresh.
    fn still(&self, d: &Due) -> bool {
        self.registry.get(&d.id).is_some_and(|v| {
            v.lifecycle_seq == d.life && v.trimmed.as_ref().map(|t| t.0) == d.trimmed
        }) && self
            .managed_worktree(&d.id)
            .is_some_and(|(_, wt)| wt == d.worktree)
            && !self.rt.get(&d.id).is_some_and(|r| r.trimming || r.deleting)
    }

    /// Starts measuring `d` when it still names its thread ([`Core::still`]).
    fn begin_measure(&mut self, d: &Due) -> bool {
        if !self.still(d) {
            return false;
        }
        self.disk.measured.insert(
            d.id.clone(),
            Measured {
                at: Instant::now(),
                at_wall: Utc::now(),
            },
        );
        true
    }

    /// Records the measurement `disk` of `d` when it moved ([`moved`]), unless `d` no longer
    /// names its thread as it is ([`Core::still`]: a measurement that may have counted what a
    /// trim deleted, or another life's worktree): then the thread is due again.
    fn note_worktree_disk(&mut self, hub: &Inner, d: &Due, disk: Option<WorktreeDisk>) {
        if !self.still(d) {
            self.disk.measured.remove(&d.id);
            return;
        }
        let Some(view) = self.registry.get(&d.id) else {
            return;
        };
        let id = d.id.as_str();
        // The box read no worktree there (parked and removed): it takes nothing now.
        let disk = disk.unwrap_or_default();
        if !moved(view.disk.as_ref(), &disk) {
            return;
        }
        let body = EventBody::WorktreeDisk(WorktreeDiskMeasured {
            bytes: disk.bytes,
            build_bytes: disk.build_bytes,
            partial: disk.partial,
            ..Default::default()
        });
        if let Err(e) = self.append(hub, id, None, Source::Hub, body) {
            tracing::warn!(thread = id, error = %format!("{e:#}"), "cannot record a disk measurement");
        }
    }
}

/// A worktree due for a measurement ([`Core::disk_due`]), and what it was then.
pub(super) struct Due {
    id: String,
    git: Arc<dyn GitOps>,
    worktree: PathBuf,
    /// The thread's last trim then.
    trimmed: Option<DateTime<Utc>>,
    /// Its `lifecycle_seq` then.
    life: u64,
}

/// A thread a disk card names.
pub(super) struct Candidate {
    id: String,
    name: String,
    /// Its build output's bytes, as last measured.
    build: u64,
    /// The life of it the card names (`lifecycle_seq`).
    lifecycle: u64,
}

/// A disk card's text: how much is left on `box_name`, and what Trim would delete.
fn disk_card_summary(box_name: &str, free: u64, threshold: u64, threads: &[Candidate]) -> String {
    let head = format!(
        "Disk low on {box_name}: {} free, under {}.",
        human_bytes(free),
        human_bytes(threshold)
    );
    if threads.is_empty() {
        return format!(
            "{head} No idle thread's worktree holds build output to trim; `twr thread list` \
             shows each worktree's size."
        );
    }
    let total: u64 = threads.iter().map(|t| t.build).sum();
    let shown: Vec<String> = threads
        .iter()
        .take(5)
        .map(|c| format!("{} {}", c.name, human_bytes(c.build)))
        .collect();
    let more = match threads.len().saturating_sub(shown.len()) {
        0 => String::new(),
        n => format!(" and {n} more"),
    };
    format!(
        "{head} Trim deletes the build output of {} idle thread(s): {}{more} ({} in all); \
         their next builds make it again.",
        threads.len(),
        shown.join(", "),
        human_bytes(total)
    )
}

impl Hub {
    /// Runs a disk round now ([`disk_round`]): what the ticker does every [`DISK_TICK`]
    /// (tests; a hub with `disk_tick` zero has none).
    pub async fn disk_round(&self) {
        disk_round(&self.inner).await;
    }

    /// `thread.trim {thread}`: deletes the build output of the thread's worktree (the user
    /// only; see the module docs). `{thread_name, thread, deleted, freed, notes}`.
    pub async fn trim_thread(&self, query: &str, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                "only the user trims a thread's worktree",
            ));
        }
        let id = self.inner.core().resolve(query)?;
        // Its own task: a caller that stops waiting leaves the trim to finish, record what it
        // did and let the thread go on.
        let (hub, source) = (Arc::clone(&self.inner), caller.source());
        tokio::spawn(async move { trim(&hub, &id, None, source, false).await })
            .await
            .map_err(|e| rpc(codes::INTERNAL, format!("the trim failed: {e}")))?
    }
}

/// Trims thread `id` for `source` ([`Hub::trim_thread`], a disk card's Trim, an idle trim:
/// `idle`, checked again as it begins, with the trim marked in the same hold of the core, and
/// only the build output its project's `idle_trim_disposable` names).
async fn trim(
    hub: &Arc<Inner>,
    id: &str,
    life: Option<u64>,
    source: Source,
    idle: bool,
) -> Result<Value, RpcError> {
    let (git, req, name, stopping) = {
        let mut core = hub.core();
        core.refuse_deleting(id)?;
        // A card names one life of a thread: one deleted and restored since is left alone.
        if let Some(life) = life {
            if core.registry.get(id).map(|v| v.lifecycle_seq) != Some(life) {
                return Err(rpc(
                    codes::CONFLICT,
                    "it was deleted (and maybe restored) since the card was raised",
                ));
            }
        }
        if idle && !core.idle_trim_due(hub, id, Utc::now()) {
            return Err(rpc(codes::CONFLICT, "it is no longer due an idle trim"));
        }
        let only = idle.then(|| {
            let project = core.registry.get(id).and_then(|v| v.project.clone());
            hub.config.idle_trim_disposable(project.as_deref()).to_vec()
        });
        let (git, req, name, stopping) = core.begin_trim(hub, id)?;
        (git, TrimRequest { only, ..req }, name, stopping)
    };
    if stopping {
        // Its idle runtime first: nothing works in the worktree while its output goes.
        let deadline = tokio::time::Instant::now() + super::ATTACH_QUIESCE_TIMEOUT;
        loop {
            let gone = hub
                .core()
                .rt
                .get(id)
                .is_none_or(|r| r.live.is_none() && !r.exiting);
            if gone {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = hub.core().end_trim(hub, id, source, None);
                return Err(rpc(
                    codes::INTERNAL,
                    format!(
                        "{name}'s runtime did not stop within {}s; nothing was trimmed",
                        super::ATTACH_QUIESCE_TIMEOUT.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let result = git.worktree_trim(req).await;
    let mut core = hub.core();
    match result {
        Ok(out) => {
            if let Err(e) = core.end_trim(hub, id, source, Some(&out)) {
                return Err(rpc(
                    codes::INTERNAL,
                    format!(
                        "{name}'s build output was deleted ({}), but the hub could not record \
                         it: {e:#}",
                        out.deleted.join(", ")
                    ),
                ));
            }
            Ok(json!({
                "thread_name": name,
                "thread": id,
                "deleted": out.deleted,
                "freed": out.freed,
                "notes": out.notes,
            }))
        }
        Err(e) => {
            let _ = core.end_trim(hub, id, source, None);
            Err(rpc(codes::CONFLICT, format!("trimming {name}: {e:#}")))
        }
    }
}

/// One round of [`disk_ticker`]: each online box's free space, then the worktrees due, one at
/// a time, then the cards.
pub(super) async fn disk_round(hub: &Arc<Inner>) {
    let boxes: Vec<(String, Arc<dyn GitOps>)> = {
        let core = hub.core();
        hub.hosts
            .iter()
            .filter(|(b, _)| core.box_online(b))
            .map(|(b, h)| (b.clone(), h.git()))
            .collect()
    };
    // The free space first, each box bounded: a box that recovered closes its card whatever
    // the walks below take.
    let mut read = Vec::new();
    for (b, git) in &boxes {
        match tokio::time::timeout(FREE_TIMEOUT, git.disk_usage(None)).await {
            Ok(Ok(r)) => {
                hub.core().note_box_disk(hub, b, &r, false);
                read.push(b.clone());
            }
            Ok(Err(e)) => {
                tracing::debug!(box_name = %b, error = %format!("{e:#}"), "cannot read a box's free disk")
            }
            Err(_) => tracing::debug!(box_name = %b, "reading a box's free disk timed out"),
        }
    }
    // Then the worktrees due, one at a time, for at most WALKS_BUDGET (the rest next round).
    let started = tokio::time::Instant::now();
    let due = hub.core().disk_due(hub);
    for d in due {
        if started.elapsed() >= WALKS_BUDGET {
            break;
        }
        if !hub.core().begin_measure(&d) {
            continue;
        }
        match d.git.disk_usage(Some(d.worktree.clone())).await {
            Ok(r) => hub.core().note_worktree_disk(hub, &d, r.worktree),
            Err(e) => {
                tracing::debug!(thread = %d.id, error = %format!("{e:#}"), "cannot measure a worktree");
                // Tried again next round.
                hub.core().disk.measured.remove(&d.id);
            }
        }
    }
    // The threads idle long enough for their project to trim them, one at a time.
    let idle = hub.core().idle_trims(hub, Utc::now());
    let mut freed = false;
    for (id, life) in idle {
        match trim(hub, &id, Some(life), Source::Hub, true).await {
            Ok(r) => {
                tracing::info!(thread = %id, freed = %r["freed"], "trimmed an idle thread's build output");
                freed = true;
            }
            Err(e) => tracing::debug!(thread = %id, error = ?e, "an idle thread was not trimmed"),
        }
    }
    // The local box's free space again after them: its card goes by what is free now.
    if freed {
        if let Some((b, git)) = boxes.iter().find(|(b, _)| b == "local") {
            if let Ok(Ok(r)) = tokio::time::timeout(FREE_TIMEOUT, git.disk_usage(None)).await {
                hub.core().note_box_disk(hub, b, &r, false);
            }
        }
    }
    // A card last, naming the threads as just measured.
    for b in read {
        hub.core().raise_if_low(hub, &b);
    }
}

/// The disk task: a round every `period` (the first one a minute after the hub starts), until
/// the hub stops or is dropped.
pub(super) async fn disk_ticker(hub: Weak<Inner>, period: Duration) {
    let start = tokio::time::Instant::now() + period.min(Duration::from_secs(60));
    let mut tick = tokio::time::interval_at(start, period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(h) = hub.upgrade() else {
            return;
        };
        if *h.state.borrow() == HubState::Stopped {
            return;
        }
        disk_round(&h).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(bytes: u64, build: u64) -> WorktreeDisk {
        WorktreeDisk {
            bytes,
            build_bytes: build,
            partial: false,
        }
    }

    #[test]
    fn small_moves_are_not_recorded() {
        const GB: u64 = 1 << 30;
        assert!(moved(None, &disk(0, 0)));
        // Under 50 MB, or under 5% of a big worktree: not recorded.
        assert!(!moved(Some(&disk(GB, 0)), &disk(GB + (40 << 20), 0)));
        assert!(!moved(
            Some(&disk(10 * GB, 0)),
            &disk(10 * GB + 400 * (1 << 20), 0)
        ));
        assert!(moved(Some(&disk(GB, 0)), &disk(GB + (60 << 20), 0)));
        assert!(moved(Some(&disk(10 * GB, 8 * GB)), &disk(10 * GB, 7 * GB)));
        let mut p = disk(GB, 0);
        p.partial = true;
        assert!(moved(Some(&disk(GB, 0)), &p));
        // Build output coming or going, however small (an idle trim goes by it).
        assert!(moved(Some(&disk(GB, 0)), &disk(GB + (10 << 20), 10 << 20)));
        assert!(moved(Some(&disk(GB, 10 << 20)), &disk(GB, 0)));
    }

    #[test]
    fn card_text_names_what_trim_deletes() {
        const GB: u64 = 1 << 30;
        let threads: Vec<Candidate> = (0..7)
            .map(|i| Candidate {
                id: format!("t{i}"),
                name: format!("n{i}"),
                build: (7 - i) * GB,
                lifecycle: 1,
            })
            .collect();
        let s = disk_card_summary("local", 3 * GB, 10 * GB, &threads);
        assert!(
            s.starts_with("Disk low on local: 3.0 GB free, under 10 GB."),
            "{s}"
        );
        assert!(s.contains("7 idle thread(s): n0 7.0 GB, n1 6.0 GB"), "{s}");
        assert!(s.contains("and 2 more (28 GB in all)"), "{s}");
        let none = disk_card_summary("gpu", GB, 10 * GB, &[]);
        assert!(
            none.contains("No idle thread's worktree holds build output"),
            "{none}"
        );
    }

    #[test]
    fn a_card_reads_back_from_its_raise() {
        let p = AttentionRaised {
            attn_id: "a".into(),
            ext: twr_ext_map(json!({DISK_KEY: {"box": "local", "threads": [
                {"thread": "t1", "name": "api", "build_bytes": 5, "lifecycle": 3},
                {"thread": "t2", "name": "web", "build_bytes": 4, "lifecycle": 9},
            ]}})),
            ..Default::default()
        };
        assert!(is_disk_card(&p.ext));
        assert_eq!(
            DiskCard::from_raised(&p),
            Some(DiskCard {
                box_name: "local".into(),
                threads: vec![("t1".into(), 3), ("t2".into(), 9)],
            })
        );
        assert!(!is_disk_card(&None));
    }
}
