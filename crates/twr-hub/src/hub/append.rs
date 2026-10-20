//! Recording events: the single writer ([`Core::append_batch`] and the appends built on it),
//! the checks every event passes before it is stored (identifier sizes, terminal-safe display
//! text, [`rejection`]), and folding stored events into the in-memory state ([`Core::track`],
//! hydrating capped payloads from the store where reducers need them).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::{Map, Value};
use twr_core::blob::{cap_body, has_capped, hydrate_with, BlobStore, CapError};
use twr_core::event::*;

use super::{
    delete, git, proposal, route, runtime_spec, trash, twr_ext, Batch, BatchEvent, Committed, Core,
    Inner, RoutedBy, MAX_ID_BYTES,
};
use crate::report;
use crate::store::{self, StagedBlobs};

/// An event the hub refuses to record: an identifier over [`MAX_ID_BYTES`], or a payload
/// [`cap_body`] cannot bring under the cap ([`CapError::TooLarge`]).
#[derive(Debug)]
pub struct InvalidEvent(pub String);

impl std::fmt::Display for InvalidEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidEvent {}

/// Why an append was refused because of the event itself (not an internal failure), if it was.
pub fn rejection(e: &anyhow::Error) -> Option<String> {
    if e.downcast_ref::<InvalidEvent>().is_some()
        || matches!(
            e.downcast_ref::<CapError>(),
            Some(CapError::TooLarge { .. })
        )
    {
        Some(e.to_string())
    } else {
        None
    }
}

/// Makes the text of `body` that clients show terminal-safe ([`report::display_text`]):
/// attention summaries and notes (newlines kept), option labels, report summaries. Every
/// append goes through here ([`Core::append_batch`]), so adapter items, the orchestrator's
/// reports and notes and the hub's own items are stored this way; an adapter event's raw
/// original stays in its raw blob. An option label an agent gave with control characters is
/// stored, shown and answered without them.
fn display_safe(body: &mut EventBody) {
    match body {
        EventBody::AttentionRaised(p) => {
            p.summary = report::display_text(&p.summary, true);
            for o in &mut p.options {
                *o = report::display_text(o, false);
            }
        }
        EventBody::AttentionAnnotated(p) => p.note = report::display_text(&p.note, true),
        EventBody::ReportReady(p) => p.summary = report::display_text(&p.summary, true),
        EventBody::ConversationUpdate(p) => {
            p.text = report::display_text(&p.text, true);
            if let Some(s) = &mut p.user_message {
                *s = report::display_text(s, true);
            }
        }
        // Paths and queries the orchestrator chose (from text it read) are shown as they are.
        EventBody::ProjectRead(p) => {
            for s in [&mut p.path, &mut p.query, &mut p.branch_of, &mut p.refused]
                .into_iter()
                .flatten()
            {
                *s = report::display_text(s, false);
            }
            p.project = report::display_text(&p.project, false);
        }
        _ => {}
    }
}

/// Checks the size of every top-level identifier (`id`, `*_id`) of `body`'s payload.
fn check_identifiers(body: &EventBody) -> anyhow::Result<()> {
    let data = body.data()?;
    if let Some(map) = data.as_object() {
        for (k, v) in map {
            if k != "id" && !k.ends_with("_id") {
                continue;
            }
            if let Some(s) = v.as_str().filter(|s| s.len() > MAX_ID_BYTES) {
                return Err(InvalidEvent(format!(
                    "{} {k} is {} bytes (max {MAX_ID_BYTES})",
                    body.kind(),
                    s.len()
                ))
                .into());
            }
        }
    }
    Ok(())
}

/// The event with its payload hydrated from `conn`'s blobs, when it has capped fields (`None`
/// otherwise, or when hydration fails, which is logged).
pub(super) fn hydrated_from_store(conn: &Connection, ev: &Event) -> Option<Event> {
    match try_hydrated(conn, ev) {
        Ok(full) => full.map(|(ev, _)| ev),
        Err(e) => {
            tracing::warn!(event = %ev.id, error = %format!("{e:#}"), "cannot hydrate a capped event");
            None
        }
    }
}

/// [`hydrated_from_store`], failing when the event has capped fields that cannot be hydrated
/// (a blob missing or unreadable), with whether every field came back (one whose blob no longer
/// matches its preview stays capped).
fn try_hydrated(conn: &Connection, ev: &Event) -> anyhow::Result<Option<(Event, bool)>> {
    // Most events have nothing moved: told without building their `data` (a hub start asks
    // this of every control event of the log).
    if !ev.body.maybe_capped() {
        return Ok(None);
    }
    let mut data = ev.body.data()?;
    if !has_capped(&data) {
        return Ok(None);
    }
    hydrate_with(&mut data, &mut |r: &str| store::get_blob(conn, r), false)?;
    let whole = !has_capped(&data);
    let body = EventBody::from_kind_data(ev.kind(), data)?;
    Ok(Some((Event { body, ..ev.clone() }, whole)))
}

/// The event with its payload hydrated whole from `conn`'s blobs, when it has capped fields
/// (`None` otherwise); failing when any field cannot be restored (a blob missing, unreadable,
/// or no longer matching its preview): what a checkpoint's fold needs (`hub::checkpoint`).
pub(super) fn hydrated_whole(conn: &Connection, ev: &Event) -> anyhow::Result<Option<Event>> {
    match try_hydrated(conn, ev)? {
        Some((ev, true)) => Ok(Some(ev)),
        Some((_, false)) => anyhow::bail!("some of its fields stay capped"),
        None => Ok(None),
    }
}

/// A named account's directory as `ext.twr` `twr` records it ([`ACCOUNT_DIR_KEY`]).
fn recorded_account_dir(twr: Option<&Map<String, Value>>) -> Option<std::path::PathBuf> {
    recorded_path(twr, ACCOUNT_DIR_KEY)
}

/// A path `ext.twr` `twr` records under `key`.
fn recorded_path(twr: Option<&Map<String, Value>>, key: &str) -> Option<std::path::PathBuf> {
    twr?.get(key)?
        .as_str()
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
}

impl Core {
    /// Bookkeeping that is not part of the reducers.
    fn track(&mut self, ev: &Event) {
        match &ev.body {
            EventBody::ThreadCreated(p) => {
                let pty_command = twr_ext(&p.ext)
                    .and_then(|t| t.get("pty_command"))
                    .filter(|c| c.is_array())
                    .map(Value::to_string);
                let acp_env = runtime_spec::acp_env(twr_ext(&p.ext));
                let account_dir = recorded_account_dir(twr_ext(&p.ext));
                let account_env_dir = recorded_path(twr_ext(&p.ext), ACCOUNT_ENV_DIR_KEY);
                let rt = self.rt(&ev.thread);
                rt.workspace = git::Workspace::from_created(p);
                rt.pty_command = pty_command;
                rt.acp_env = acp_env;
                rt.account_dir = account_dir;
                rt.account_env_dir = account_env_dir;
            }
            // Moved to another runtime or account (`hub::switch`): what that one needs,
            // recorded like `thread.created`'s (a `pty` thread's command goes; a move is never
            // to `pty`). A live runtime is the previous one, whatever its name
            // (`hub::restart`). A move between accounts of one runtime keeps its agent unless
            // the event names one (as the reducer keeps `acp_command`).
            EventBody::ThreadModelChanged(p) if p.runtime.is_some() => {
                let same_runtime = p.previous_runtime == p.runtime;
                let account_only = p.account_changed && same_runtime;
                let names_agent = twr_ext(&p.ext).is_some_and(|t| t.contains_key(ACP_COMMAND_KEY));
                let acp_env = runtime_spec::acp_env(twr_ext(&p.ext));
                let account_dir = recorded_account_dir(twr_ext(&p.ext));
                let account_env_dir = recorded_path(twr_ext(&p.ext), ACCOUNT_ENV_DIR_KEY);
                let rt = self.rt(&ev.thread);
                rt.pty_command = None;
                if !account_only || names_agent {
                    rt.acp_env = acp_env;
                }
                // An account move records the new account's directory (none for the native).
                if p.account_changed {
                    rt.account_dir = account_dir;
                    rt.account_env_dir = account_env_dir;
                }
                if let Some(s) = rt.runtime_settings.as_mut() {
                    s.moved = true;
                    s.account_moved |= account_only;
                }
                // The previous session's settings are no session's of the thread now: the new
                // runtime's first start records its own (an attach meanwhile is refused, the
                // thread having no session).
                rt.session_settings = None;
            }
            // Gone (`hub::delete`): its runtime was stopped and its items resolved with it.
            // What a restore needs again is set aside (`hub::trash`), with its trash item.
            EventBody::ThreadDeleted(d) => {
                let mut saved = self
                    .rt
                    .remove(&ev.thread)
                    .map(trash::DeletedRt::of)
                    .unwrap_or_default();
                saved.item = d.trash.clone();
                self.deleted_rt.insert(ev.thread.clone(), saved);
                if let Some(item) = &d.trash {
                    self.items_deleted.insert(item.clone());
                }
                let at = u64::try_from(ev.ts.timestamp_millis()).unwrap_or(0);
                self.trash_seen(&ev.thread, d, at);
            }
            // Back from the trash: its bookkeeping as it was, its branch as restored.
            EventBody::ThreadRestored(p) => {
                self.items_restored.insert(
                    p.item.clone(),
                    trash::Restored {
                        name: p.name.clone(),
                        at_ms: u64::try_from(ev.ts.timestamp_millis()).unwrap_or(0),
                    },
                );
                let saved = self.deleted_rt.remove(&ev.thread).unwrap_or_default();
                let rt = self.rt(&ev.thread);
                saved.restore(rt, p);
            }
            // A runtime reports its process; the hub's own `thread.resumed` (`thread.resume`)
            // starts none. The user's is an explicit resume: it releases a queue a crash held
            // (as `thread.resume` does live); the orchestrator's does not.
            EventBody::ThreadResumed(_) if !matches!(ev.source, Source::Adapter(_)) => {
                if matches!(ev.source, Source::User(_)) {
                    let rt = self.rt(&ev.thread);
                    rt.queue_held = false;
                    rt.crash_hold = None;
                }
            }
            EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p) => {
                let ext = twr_ext(&p.ext);
                let pid = ext
                    .and_then(|e| e.get("pid"))
                    .and_then(Value::as_u64)
                    .and_then(|p| u32::try_from(p).ok());
                let start_time = ext
                    .and_then(|e| e.get("start_time"))
                    .and_then(Value::as_u64);
                let exe = ext
                    .and_then(|e| e.get("exe"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // The settings it runs in: as the hub stamped them (`hub::restart`), else what
                // the hub started it with, else (a log from before) the thread's settings then.
                let stamped = super::restart::stamped(p, &ev.source);
                let thread_settings = self
                    .registry
                    .get(&ev.thread)
                    .map(super::restart::RunSettings::of);
                let rt = self.rt(&ev.thread);
                rt.process_live = true;
                // A runtime runs again (as `start_runtime` says live): a crash's hold is over.
                rt.queue_held = false;
                rt.crash_hold = None;
                rt.pid = pid;
                rt.start_time = start_time;
                rt.exe = exe;
                rt.runtime_settings = stamped.or(rt.runtime_settings.take()).or(thread_settings);
                rt.session_settings = rt.runtime_settings.clone();
            }
            // Attaching quiesces the runtime first: its process is gone too.
            EventBody::ThreadStopped(_)
            | EventBody::ThreadCrashed(_)
            | EventBody::ThreadParked(_)
            | EventBody::ThreadAttached(_) => {
                // A worker whose crash took a turn down (`ThreadView::crashed_turn`; an idle
                // runtime's exit fails nothing).
                let failed_worker = self
                    .registry
                    .get(&ev.thread)
                    .is_some_and(|v| !v.is_orchestrator() && v.crashed_turn.is_some());
                // The crash's control position (`apply`: set for a control event).
                let gseq = self.gseq;
                let rt = self.rt(&ev.thread);
                rt.process_live = false;
                rt.pid = None;
                rt.start_time = None;
                rt.exe = None;
                rt.runtime_settings = None;
                // The store drops the stop request in the same transaction.
                rt.stop_requested = false;
                rt.closed_runtimes += 1;
                // A worker's crash that failed a turn holds its queue until the user acts
                // (`hub::retry`): every ordinary dispatch checks the hold, a due automatic
                // retry aside. Folded from the log, so a hub restart and a runner's
                // reconnection keep it (a runtime that died with the previous hub is the
                // exception: `close_stale_runtimes`).
                if matches!(ev.body, EventBody::ThreadCrashed(_)) && failed_worker {
                    rt.queue_held = true;
                    rt.crash_hold = Some(gseq);
                }
            }
            EventBody::TurnStarted(_) => self.rt(&ev.thread).final_text = None,
            // The turn that carried the resume of a session the thread moved back to failed
            // (`ThreadView::resume_failed_turn`, `hub::switch`): the runtime that tried has no
            // session of the thread, whose next turn starts a new one. A live one is replaced
            // like a runtime the thread moved away from (`hub::restart`), never sent that turn.
            EventBody::TurnEnded(_) => {
                let failed = self
                    .registry
                    .get(&ev.thread)
                    .is_some_and(|v| v.resume_failed_turn.is_some_and(|t| ev.turn == Some(t)));
                if failed {
                    if let Some(s) = self.rt(&ev.thread).runtime_settings.as_mut() {
                        s.moved = true;
                    }
                }
            }
            // A trash item gone from its box (`hub::trash`): no longer listed, and when it is
            // the deleted thread's own, the thread is not restorable any more; what its agent
            // did is archived (`hub::prune`), so nothing folded from that is kept.
            EventBody::TrashPurged(p) if ev.thread == HUB_THREAD && ev.source == Source::Hub => {
                if self
                    .trash
                    .get(&p.item)
                    .is_some_and(|s| s.thread == p.thread)
                {
                    self.trash.remove(&p.item);
                }
                if self.registry.is_deleted(&p.thread) {
                    if let Some(d) = self
                        .deleted_rt
                        .get_mut(&p.thread)
                        .filter(|d| d.item.as_deref() == Some(p.item.as_str()))
                    {
                        d.purged = true;
                        self.activity.remove(&p.thread);
                    }
                }
            }
            EventBody::AttentionRaised(p) => {
                self.known_attn.insert(p.attn_id.clone());
                if matches!(ev.source, Source::Adapter(_)) {
                    self.adapter_attn.insert(p.attn_id.clone(), ev.turn);
                }
                if ev.thread == HUB_THREAD {
                    if let Some(pending) = route::PendingRoute::from_raised(p) {
                        self.hub_questions.insert(p.attn_id.clone(), pending);
                    } else if ev.source == Source::Hub && is_delete_card(&ev.thread, &p.ext) {
                        if let Some(card) = delete::DeleteCard::from_raised(p) {
                            self.delete_cards.insert(p.attn_id.clone(), card);
                        }
                    } else if ev.source == Source::Hub && super::disk::is_disk_card(&p.ext) {
                        if let Some(card) = super::disk::DiskCard::from_raised(p) {
                            self.disk.cards.insert(p.attn_id.clone(), card);
                        }
                    } else if ev.source == Source::Hub && is_confirm_card(&ev.thread, &p.ext) {
                        if let Some(card) = proposal::Proposal::from_raised(p) {
                            self.hub_cards.insert(p.attn_id.clone(), card);
                        }
                    }
                } else if p.reason == AttentionReason::Error && ev.source == Source::Hub {
                    self.hub_errors
                        .insert(p.attn_id.clone(), (ev.thread.clone(), ev.turn));
                }
            }
            EventBody::AttentionResolved(p) => {
                self.adapter_attn.remove(&p.attn_id);
                self.hub_questions.remove(&p.attn_id);
                self.delete_cards.remove(&p.attn_id);
                self.disk.cards.remove(&p.attn_id);
                self.track_card(p);
                self.hub_errors.remove(&p.attn_id);
            }
            _ => {}
        }
        // Before the orchestrator's: a worker's idle line waits for its wake (`hub::wake`).
        self.track_wake(ev);
        self.track_orchestrator(ev);
        self.track_context(ev);
        self.track_stall(ev);
        self.track_after(ev);
        self.track_idle_offer(ev);
        self.track_activity(ev);
    }

    /// Folds `ev`; for a control event `self.gseq` is its own control position (set by the
    /// callers first), which is the revision of an `attention.raised`.
    pub(super) fn apply(&mut self, ev: &Event) {
        // (No new key for a thread already known: this runs for every event of the log.)
        match self.seqs.get_mut(&ev.thread) {
            Some(seq) => *seq = ev.seq,
            None => {
                self.seqs.insert(ev.thread.clone(), ev.seq);
            }
        }
        match self.last_ts.get_mut(&ev.thread) {
            Some(ts) => *ts = ev.ts,
            None => {
                self.last_ts.insert(ev.thread.clone(), ev.ts);
            }
        }
        self.registry.apply(ev);
        self.attention
            .apply_at(ev, ev.is_control().then_some(self.gseq));
        self.usage.apply(ev);
        self.accounts.apply(ev);
        if ev.thread == twr_core::event::HUB_THREAD
            && matches!(
                ev.body,
                EventBody::AccountAdded(_)
                    | EventBody::AccountRemoved(_)
                    | EventBody::AccountStatus(_)
            )
        {
            self.account_events.push((self.gseq, ev.clone()));
        }
        self.track(ev);
    }

    /// The single write path. Refuses (with [`InvalidEvent`] / [`CapError::TooLarge`], see
    /// [`rejection`]) events with oversized identifiers or payloads that cannot be capped.
    /// The reducers fold the full body, not the capped one: previews are for the wire and the
    /// log, never state.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn append_ex(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: Option<u64>,
        source: Source,
        body: EventBody,
        raw: Option<String>,
        dequeue: Option<i64>,
    ) -> anyhow::Result<Event> {
        self.append_staged(
            hub,
            thread,
            turn,
            source,
            body,
            raw,
            dequeue,
            StagedBlobs::default(),
        )
    }

    /// [`Core::append_ex`] that also stores the blobs already in `staged` (in the same
    /// transaction as the event).
    #[allow(clippy::too_many_arguments)]
    fn append_staged(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: Option<u64>,
        source: Source,
        body: EventBody,
        raw: Option<String>,
        dequeue: Option<i64>,
        staged: StagedBlobs,
    ) -> anyhow::Result<Event> {
        let mut batch = Batch {
            dequeue,
            staged,
            ..Batch::default()
        };
        batch.events.push(BatchEvent {
            thread: thread.to_string(),
            turn,
            source,
            body,
            raw,
        });
        let done = self.append_batch(hub, batch)?;
        done.events
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("a one-event batch recorded no event"))
    }

    /// Records `batch` atomically: every event (seq, ULID, timestamp, capped
    /// payload), its blobs, the dequeued prompt and the queued prompts are one SQLite
    /// transaction ([`Store::append_batch`]), so a crash or a write failure leaves either all
    /// of it in the log or none of it (and the hub state untouched). Only after the commit are
    /// the events folded into the reducers, in order, and broadcast. A refused event
    /// ([`rejection`]) refuses the whole batch.
    ///
    /// [`Store::append_batch`]: crate::store::Store::append_batch
    pub(super) fn append_batch(&mut self, hub: &Inner, batch: Batch) -> anyhow::Result<Committed> {
        let Batch {
            mut events,
            prompts,
            dequeue,
            staged,
            runner_ack,
            remote_cmds,
            remote_done,
            stop_requests,
            drop_queues,
            at,
        } = batch;
        // Original event indices remain stable; visible updates commit with their facts.
        let updates = self.conversation_updates(&events);
        events.extend(updates);
        // Activity on a thread with an open stalled item resolves it in the same transaction
        // (`hub::stall`), so no crash can leave it open behind the activity.
        let resolutions = self.stall_resolutions(&events);
        events.extend(resolutions);
        // An account whose directory the batch changes has its known usage cleared with it
        // (`hub::usage`).
        let clears = self.usage_clears(&events);
        events.extend(clears);
        // Per-thread seqs (and timestamps) as they will be once the earlier events of the batch
        // are in.
        let mut seqs: HashMap<String, u64> = HashMap::new();
        let mut times: HashMap<String, DateTime<Utc>> = HashMap::new();
        let mut prepared: Vec<(Event, EventBody, Option<String>)> =
            Vec::with_capacity(events.len());
        for mut e in events {
            // The orchestrator's memory calls are recorded without their text (`hub::memory`).
            self.redact_memory_call(&e.thread, &mut e.body, &mut e.raw, &e.source);
            if self.is_orch(&e.thread) {
                if let EventBody::TurnStarted(p) = &mut e.body {
                    let ext = p.ext.get_or_insert_with(JsonMap::new);
                    let twr = ext.entry("twr").or_insert_with(|| serde_json::json!({}));
                    if let Some(twr) = twr.as_object_mut() {
                        twr.insert(
                            "prompt_fingerprint".into(),
                            serde_json::json!(super::orch::prompt_fingerprint()),
                        );
                    }
                }
            }
            // A worker's request carries what its agent last said and the turn's prompt
            // (`hub::activity`).
            if let EventBody::AttentionRaised(p) = &mut e.body {
                if matches!(e.source, Source::Adapter(_))
                    && e.thread != HUB_THREAD
                    && !self.is_orch(&e.thread)
                    && matches!(
                        p.reason,
                        AttentionReason::Permission | AttentionReason::Question
                    )
                {
                    if let Some(context) = self.request_context(&e.thread) {
                        super::activity::add_context(&mut p.ext, context);
                    }
                }
            }
            // Nothing more is recorded on a deleted thread (`hub::delete`) but its restore
            // (`hub::trash`).
            if self.registry.is_deleted(&e.thread)
                && !matches!(e.body, EventBody::ThreadRestored(_))
            {
                return Err(InvalidEvent(format!("thread {} was deleted", e.thread)).into());
            }
            display_safe(&mut e.body);
            check_identifiers(&e.body)?;
            let full_final_text = match &e.body {
                EventBody::AgentText(t) if t.is_final => Some(t.text.clone()),
                _ => None,
            };
            let capped = cap_body(&e.body, &staged)?;
            // The reducers always fold the original body: it is what hydrating the stored
            // event gives back. (Whether a field was capped cannot be told from the staged
            // blob count: staging deduplicates, so a second event of the batch sharing a large
            // field adds no blob.)
            let full_body = e.body;
            let raw_ref = e.raw.map(|r| staged.put(r.as_bytes())).transpose()?;
            let last = seqs
                .get(&e.thread)
                .or_else(|| self.seqs.get(&e.thread))
                .copied()
                .unwrap_or(0);
            seqs.insert(e.thread.clone(), last + 1);
            let mut ev = Event::new(e.thread, last + 1, e.turn, e.source, capped);
            if let Some(at) = at {
                let floor = times
                    .get(&ev.thread)
                    .or_else(|| self.last_ts.get(&ev.thread));
                ev.ts = Core::event_time(at, floor.copied(), ev.ts);
            }
            times.insert(ev.thread.clone(), ev.ts);
            ev.raw = raw_ref;
            prepared.push((ev, full_body, full_final_text));
        }
        let enqueue = prompts
            .into_iter()
            .map(|p| {
                let routed_by =
                    match p.routed_by {
                        None => None,
                        Some(RoutedBy::Id(id)) => Some(id),
                        Some(RoutedBy::Event(i)) => {
                            Some(prepared.get(i).map(|(ev, _, _)| ev.id.clone()).ok_or_else(
                                || anyhow::anyhow!("routed_by names no event of the batch"),
                            )?)
                        }
                    };
                Ok(store::NewPrompt {
                    thread: p.thread,
                    prompt: p.prompt,
                    from: p.from,
                    routed_by,
                    source: p.source,
                    said_after: p.said_after,
                    user_send: p.user_send,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let blobs = staged.take();
        let commit = self.store.append_batch(store::AppendBatch {
            events: prepared
                .iter()
                .map(|(ev, _, _)| (ev, ev.is_control()))
                .collect(),
            blobs: &blobs,
            dequeue,
            enqueue,
            runner_ack: runner_ack.as_ref(),
            defer_sync: self.defer_ack_sync,
            remote_cmds: &remote_cmds,
            remote_done: &remote_done,
            stop_requests: &stop_requests,
            drop_queues: &drop_queues,
        })?;
        // A compaction asked for goes with the queue (`hub::compact`).
        for t in &drop_queues {
            if let Some(r) = self.rt.get_mut(t) {
                r.compact = None;
            }
        }
        let mut out = Vec::with_capacity(prepared.len());
        for ((ev, full_body, final_text), gseq) in prepared.into_iter().zip(commit.gseqs) {
            // Reducers see the event's own gseq as the head (the relay guard orders by it).
            if let Some(g) = gseq {
                self.gseq = g;
            }
            let full = Event {
                body: full_body,
                ..ev.clone()
            };
            self.apply(&full);
            if let Some(g) = gseq {
                self.feed_hook(&full, g);
            }
            self.feed_after(&full);
            self.task_progress_after(&full);
            if let Some(text) = final_text {
                self.rt(&ev.thread).final_text = Some(text);
            }
            let shared = Arc::new(ev.clone());
            if let Some(g) = gseq {
                // Folded for the snapshots (`hub::subscribe`) before any subscriber has it.
                self.client.apply(&full, Some(g));
                let _ = hub.control_tx.send((g, Arc::clone(&shared)));
            }
            let _ = hub.events_tx.send(shared);
            out.push(ev);
        }
        if std::mem::take(&mut self.orch.wake) {
            let _ = hub.notify_tx.send(());
        }
        Ok(Committed {
            events: out,
            queued: commit.queued,
            dropped: commit.dropped,
        })
    }

    /// The `ts` of an event that happened `at` (a runner's capture time) and is recorded
    /// `now`: `at`, but never before the thread's previous event (`floor`: the log stays in
    /// time order per thread) nor after `now` (a runner clock ahead of the hub's).
    pub(super) fn event_time(
        at: DateTime<Utc>,
        floor: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> DateTime<Utc> {
        let at = at.min(now);
        floor.map_or(at, |f| at.max(f.min(now)))
    }

    pub(super) fn append(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: Option<u64>,
        source: Source,
        body: EventBody,
    ) -> anyhow::Result<Event> {
        self.append_ex(hub, thread, turn, source, body, None, None)
    }

    /// Commits a runner frame that adds no event (a duplicate-free frame the hub drops): only
    /// its watermark moves.
    pub(super) fn commit_ack(
        &mut self,
        hub: &Inner,
        ack: Option<&store::RunnerAck>,
    ) -> anyhow::Result<()> {
        if let Some(a) = ack {
            self.append_batch(
                hub,
                Batch {
                    runner_ack: Some(a.clone()),
                    ..Batch::default()
                },
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::config::Config;

    use super::super::{Hub, HubOptions};

    /// Two events of one batch sharing a large field: the second stages no new blob, yet
    /// live reducers must fold its full body, exactly like a replay (which hydrates it).
    #[tokio::test]
    async fn live_reducers_fold_full_bodies_even_when_blobs_are_shared() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let long = "s".repeat(10_000);
        let summaries = |hub: &Hub| {
            let core = hub.inner.core();
            ["a1", "a2"]
                .map(|id| core.attention.get(id).map(|i| i.summary.len()))
                .to_vec()
        };
        {
            let hub = Hub::open(HubOptions::new(&db, Config::default()))
                .await
                .unwrap();
            {
                let inner = &hub.inner;
                let mut core = inner.core();
                let mut batch = Batch::default();
                for id in ["a1", "a2"] {
                    batch.push(
                        "t",
                        None,
                        Source::Hub,
                        EventBody::AttentionRaised(AttentionRaised {
                            attn_id: id.into(),
                            reason: AttentionReason::Error,
                            summary: long.clone(),
                            options: vec!["dismiss".into()],
                            ..Default::default()
                        }),
                    );
                }
                let done = core.append_batch(inner, batch).unwrap();
                // Both were capped in the log.
                for ev in &done.events {
                    assert!(has_capped(&ev.body.data().unwrap()));
                }
            }
            assert_eq!(summaries(&hub), vec![Some(10_000), Some(10_000)]);
        }
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        assert_eq!(summaries(&hub), vec![Some(10_000), Some(10_000)]);
    }
}
