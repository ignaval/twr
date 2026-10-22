//! Checkpoints of the hub's folded state, so that a hub start folds only the end of the log.
//!
//! A hub that starts rebuilds its state by folding every event of the log ([`Core::apply`]);
//! with a long log that takes seconds. A checkpoint (`checkpoints`, [`crate::store`]) holds that
//! state as of one control event ([`FoldSnapshot`], as JSON): the start loads the newest one it
//! can use and folds only the events after that event ([`start`]). The log stays the truth: a
//! checkpoint is a cache, which can be dropped without loss ([`store::drop_checkpoints`]).
//!
//! **Written from the log alone.** The live state holds what no replay rebuilds (runtimes,
//! leases, the orchestrator's token, counters), so no checkpoint is ever taken of it. A
//! background task ([`checkpoint_ticker`]) replays the log on connections of its own: from the
//! newest checkpoint it can use (or from nothing), it folds the events after it up to the log's
//! last control event, and records the result in one short transaction ([`write`]), keeping the
//! newest [`store::CHECKPOINTS_KEPT`]. It looks shortly after the hub opens and then every
//! [`CHECKPOINT_TICK`], and writes only once [`CHECKPOINT_EVERY`] events or more came after the
//! newest checkpoint. It takes no core lock.
//!
//! **What one covers.** Every event up to its `last_event`, a control event (control rows are
//! never deleted). Its place in the log is looked up by its id each time (a `VACUUM` may
//! renumber rows, never reorder them), and its control gseq must be the checkpoint's. The
//! events after it are folded as a replay folds them ([`Core::fold_events`]): hydrated where the
//! reducers read whole payloads, and the notification hook's policy recalling control events.
//!
//! **Who may use one.** A hub of the same [`FORMAT`], the same build (version and a hash of the
//! fold's sources, [`build_key`]: another build may fold otherwise), with the same settings of
//! what the fold reads ([`config_key`]: whether `[notify] command` is set, and `[notify]
//! events`, the hook's policy being part of the fold), and a [`checksum`] that still matches the
//! row. Anything else (another format, build or settings, a state changed since it was written
//! or that does not read, a last event the log does not have at that control position) is
//! logged as a warning, and the next older checkpoint, then the whole log, is folded instead.
//!
//! **Only from whole payloads.** A replay that cannot hydrate a capped payload folds its
//! preview (`hub::append`, logged); the writer fails instead ([`Core::fold_events_strict`]), so
//! that no checkpoint keeps what a later replay would fold whole.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Weak;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use twr_core::config::NotifyConfig;
use twr_core::notify::Policy;
use twr_core::reduce::{AttentionStack, Registry};
use twr_core::usage::UsageView;

use super::{
    activity, after, delete, disk, hook, orch, proposal, route, stall, trash, wake, Core, HubState,
    Inner, ThreadRt,
};
use crate::store::{self, CheckpointMeta, NewCheckpoint, Store};

/// The version of [`FoldSnapshot`]'s serialization. A checkpoint of another one is not used.
pub(super) const FORMAT: i64 = 1;

/// How many events must come after the newest checkpoint (or be in the log, without one) before
/// another one is written.
pub const CHECKPOINT_EVERY: u64 = 5_000;

/// How often the hub looks whether a checkpoint is due.
pub const CHECKPOINT_TICK: Duration = Duration::from_secs(3600);

/// How long after the hub opened it first looks (the tick when that is shorter): soon, but not
/// while clients catch up with the hub that just started.
const FIRST_LOOK: Duration = Duration::from_secs(30);

/// How a hub start rebuilt its state ([`Core::replay_with`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Replayed {
    /// The checkpoint it started from (its id), if any.
    pub(super) checkpoint: Option<i64>,
    /// How many events it folded (after that checkpoint).
    pub(super) events: usize,
    /// The checkpoints it did not use (newer than the one it used), with why.
    pub(super) refused: Vec<(i64, String)>,
}

/// The checkpoint a fold starts from: its id, and the row its last event is at now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Start {
    pub(super) id: i64,
    pub(super) row: i64,
}

/// Every field of [`Core`] the fold builds, and only those: the state a checkpoint holds.
/// Serialized through [`serde_json::Value`] (objects sorted by key; hash sets are sorted with
/// [`twr_core::sorted`]), so that equal states give equal bytes. (Tests also compare two by
/// their fields' own `PartialEq`, whatever their serialization leaves out.)
#[derive(Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
pub(super) struct FoldSnapshot {
    registry: Registry,
    attention: AttentionStack,
    usage: UsageView,
    seqs: HashMap<String, u64>,
    last_ts: HashMap<String, DateTime<Utc>>,
    /// The control position of the checkpoint's last event.
    gseq: u64,
    #[serde(serialize_with = "twr_core::sorted::set")]
    known_attn: HashSet<String>,
    adapter_attn: HashMap<String, Option<u64>>,
    rt: HashMap<String, ThreadRt>,
    orch: orch::OrchState,
    hub_questions: HashMap<String, route::PendingRoute>,
    hub_cards: HashMap<String, proposal::Proposal>,
    delete_cards: HashMap<String, delete::DeleteCard>,
    disk: disk::DiskState,
    /// The notification hook's policy (`None` without `[notify] command`).
    hook: Option<Policy>,
    after: after::AfterState,
    card_outcomes: HashMap<String, String>,
    context_hashes: HashMap<String, String>,
    project_contexts: HashMap<String, String>,
    hub_errors: HashMap<String, (String, Option<u64>)>,
    stalled: HashMap<String, String>,
    wake: wake::WakeState,
    hung: stall::HungState,
    went_on: stall::WentOn,
    activity: HashMap<String, activity::Activity>,
    trash: BTreeMap<String, trash::Seen>,
    deleted_rt: HashMap<String, trash::DeletedRt>,
    #[serde(serialize_with = "twr_core::sorted::set")]
    items_deleted: HashSet<String>,
    items_restored: HashMap<String, trash::Restored>,
}

impl FoldSnapshot {
    /// The state as a checkpoint stores it.
    pub(super) fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&serde_json::to_value(self)?)?)
    }

    /// The state a checkpoint stored.
    pub(super) fn from_bytes(bytes: &[u8]) -> anyhow::Result<FoldSnapshot> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

impl Core {
    /// The folded state, and the store. Every field of [`Core`] is named here: the folded ones
    /// go into the snapshot, the others are memory only (see [`Core`]).
    pub(super) fn into_snapshot(self) -> (Store, FoldSnapshot) {
        let Core {
            store,
            registry,
            attention,
            seqs,
            last_ts,
            gseq,
            known_attn,
            adapter_attn,
            rt,
            // Memory only: runtime starts, attach leases, the shutdown, counters.
            generation: _,
            leases: _,
            shutting_down: _,
            dropped_events: _,
            sanitized_events: _,
            persist_retries: _,
            // Memory only: what the hub did not record of its adapters' task updates
            // (`hub::coalesce`).
            coalesced_events: _,
            task_progress: _,
            // Memory only: what a replay read for the compacted backlog (`hub::subscribe`).
            compact_rows: _,
            orch,
            hub_questions,
            hub_cards,
            delete_cards,
            disk,
            // Memory only: jobs never outlive their hub (`hub::jobs`).
            jobs: _,
            hook,
            after,
            card_outcomes,
            context_hashes,
            project_contexts,
            // Memory only: what was read and resolved since the hub started (`hub::context`).
            context_files: _,
            context_roots: _,
            hub_errors,
            stalled,
            wake,
            hung,
            went_on,
            activity,
            // Memory only: what operations in progress hold.
            reserved_names: _,
            trash,
            deleted_rt,
            trash_busy: _,
            items_deleted,
            items_restored,
            // Memory only: the boxes' connections.
            online_boxes: _,
            switching_boxes: _,
            draining_boxes: _,
            defer_ack_sync: _,
            usage,
            replayed: _,
        } = self;
        let snap = FoldSnapshot {
            registry,
            attention,
            usage,
            seqs,
            last_ts,
            gseq,
            known_attn,
            adapter_attn,
            rt,
            orch,
            hub_questions,
            hub_cards,
            delete_cards,
            disk,
            hook: hook.map(|f| f.policy),
            after,
            card_outcomes,
            context_hashes,
            project_contexts,
            hub_errors,
            stalled,
            wake,
            hung,
            went_on,
            activity,
            trash,
            deleted_rt,
            items_deleted,
            items_restored,
        };
        (store, snap)
    }

    /// The state `snap` holds, on `store`, with the notification hook's feed `hook` (its
    /// policy as `snap` has it: a checkpoint is used only with the same `[notify]` settings).
    pub(super) fn from_snapshot(
        store: Store,
        hook: Option<hook::HookFeed>,
        snap: FoldSnapshot,
    ) -> anyhow::Result<Core> {
        let mut core = Core::empty(store, hook)?;
        let FoldSnapshot {
            registry,
            attention,
            usage,
            seqs,
            last_ts,
            gseq,
            known_attn,
            adapter_attn,
            rt,
            orch,
            hub_questions,
            hub_cards,
            delete_cards,
            disk,
            hook,
            after,
            card_outcomes,
            context_hashes,
            project_contexts,
            hub_errors,
            stalled,
            wake,
            hung,
            went_on,
            activity,
            trash,
            deleted_rt,
            items_deleted,
            items_restored,
        } = snap;
        core.registry = registry;
        core.attention = attention;
        core.usage = usage;
        core.seqs = seqs;
        core.last_ts = last_ts;
        core.gseq = gseq;
        core.known_attn = known_attn;
        core.adapter_attn = adapter_attn;
        core.rt = rt;
        core.orch = orch;
        core.hub_questions = hub_questions;
        core.hub_cards = hub_cards;
        core.delete_cards = delete_cards;
        core.disk = disk;
        if let (Some(feed), Some(policy)) = (core.hook.as_mut(), hook) {
            feed.policy = policy;
        }
        core.after = after;
        core.card_outcomes = card_outcomes;
        core.context_hashes = context_hashes;
        core.project_contexts = project_contexts;
        core.hub_errors = hub_errors;
        core.stalled = stalled;
        core.wake = wake;
        core.hung = hung;
        core.went_on = went_on;
        core.activity = activity;
        core.trash = trash;
        core.deleted_rt = deleted_rt;
        core.items_deleted = items_deleted;
        core.items_restored = items_restored;
        Ok(core)
    }
}

/// The build a checkpoint is written by and used by: twr's version and a hash of the sources
/// the fold was built from and of the compiler, target and settings that built it
/// (`TWR_FOLD_SOURCES`, made by `build.rs` at build time: development
/// builds of one commit share a version, and reading the executable to tell them apart would
/// cost a hub start the read of the whole file).
pub(super) fn build_key() -> String {
    format!("{} src {}", twr_core::version(), env!("TWR_FOLD_SOURCES"))
}

/// The checksum of a checkpoint: SHA-256 over every field the writer recorded and its state, so
/// that a change to any of them since (a flipped bit that leaves the JSON readable) shows.
pub(super) fn checksum(
    format: i64,
    build: &str,
    config: &str,
    gseq: u64,
    last_event: &str,
    state: &[u8],
) -> String {
    let format = format.to_string();
    let gseq = gseq.to_string();
    let mut buf = Vec::with_capacity(state.len() + 256);
    for part in [
        format.as_bytes(),
        build.as_bytes(),
        config.as_bytes(),
        gseq.as_bytes(),
        last_event.as_bytes(),
        state,
    ] {
        buf.extend_from_slice(&(part.len() as u64).to_le_bytes());
        buf.extend_from_slice(part);
    }
    twr_core::blob::blob_ref(&buf)
}

/// What the fold reads of the hub's settings: the notification hook's events, when it has a
/// command (`hook`: its feed).
pub(super) fn config_key(hook: Option<&hook::HookFeed>) -> String {
    serde_json::json!({ "notify": hook.map(|f| f.policy.wanted()) }).to_string()
}

/// Why checkpoint `meta` cannot be used by this build (`build`) with settings `config`, if it
/// cannot; else where its last event is now.
fn usable(
    conn: &rusqlite::Connection,
    meta: &CheckpointMeta,
    build: &str,
    config: &str,
) -> Result<i64, String> {
    if meta.format != FORMAT {
        return Err(format!("format {}, not {FORMAT}", meta.format));
    }
    if meta.build != build {
        return Err(format!("written by {}, not {build}", meta.build));
    }
    if meta.config != config {
        return Err(format!(
            "written with other settings ({}, now {config})",
            meta.config
        ));
    }
    match store::event_position(conn, &meta.last_event) {
        Err(e) => Err(format!("its last event cannot be looked up: {e:#}")),
        Ok(None) => Err(format!(
            "its last event {} is not in the log",
            meta.last_event
        )),
        Ok(Some((row, Some(g)))) if g == meta.gseq => Ok(row),
        Ok(Some((_, g))) => Err(format!(
            "its last event {} is at control position {g:?}, not {}",
            meta.last_event, meta.gseq
        )),
    }
}

/// Notes that checkpoint `id` is not used, and why.
fn refuse(refused: &mut Vec<(i64, String)>, id: i64, why: String) {
    tracing::warn!(checkpoint = id, "not using checkpoint {id}: {why}");
    twr_core::trace::mark_with(|| format!("hub: checkpoint {id} not used: {why}"));
    refused.push((id, why));
}

/// Each checkpoint of `conn`, newest first, with where its last event is now when this build
/// can use it with settings `config`, else why not ([`usable`]).
type Candidates = Vec<(CheckpointMeta, Result<i64, String>)>;

/// The [`Candidates`] of `conn`.
fn candidates(conn: &rusqlite::Connection, config: &str) -> anyhow::Result<Candidates> {
    let build = build_key();
    Ok(store::checkpoints(conn)?
        .into_iter()
        .map(|meta| {
            let row = usable(conn, &meta, &build, config);
            (meta, row)
        })
        .collect())
}

/// The newest of `candidates` that can be used and whose state is the one written and reads:
/// where it ends, and its state. Those passed over on the way are noted in `refused`.
fn first_valid(
    conn: &rusqlite::Connection,
    candidates: &Candidates,
    refused: &mut Vec<(i64, String)>,
) -> Option<(Start, FoldSnapshot)> {
    for (meta, row) in candidates {
        let row = match row {
            Ok(row) => row,
            Err(why) => {
                refuse(refused, meta.id, why.clone());
                continue;
            }
        };
        let snap = store::checkpoint_state(conn, meta.id).and_then(|s| {
            let s = s.context("it is gone")?;
            let sum = checksum(
                meta.format,
                &meta.build,
                &meta.config,
                meta.gseq,
                &meta.last_event,
                &s,
            );
            anyhow::ensure!(sum == meta.checksum, "its checksum does not match");
            let snap = FoldSnapshot::from_bytes(&s)?;
            anyhow::ensure!(
                snap.gseq == meta.gseq,
                "it is at control position {}, not {}",
                snap.gseq,
                meta.gseq
            );
            Ok(snap)
        });
        match snap {
            Ok(snap) => {
                let start = Start {
                    id: meta.id,
                    row: *row,
                };
                return Some((start, snap));
            }
            Err(e) => refuse(refused, meta.id, format!("its state does not read: {e:#}")),
        }
    }
    None
}

/// The state to fold the rest of the log onto, on `store`: that of `valid` ([`first_valid`]),
/// and where it ends; else that of an empty log.
fn starting_core(
    store: Store,
    hook: Option<hook::HookFeed>,
    valid: Option<(Start, FoldSnapshot)>,
) -> anyhow::Result<(Core, Option<Start>)> {
    Ok(match valid {
        Some((start, snap)) => (Core::from_snapshot(store, hook, snap)?, Some(start)),
        None => (Core::empty(store, hook)?, None),
    })
}

/// The state a hub start folds the log's last events onto: the newest checkpoint of `store`
/// it can use, and where that checkpoint ends; else the state of an empty log. Every
/// checkpoint passed over is noted in `refused` (and logged).
pub(super) fn start(
    store: Store,
    hook: Option<hook::HookFeed>,
    refused: &mut Vec<(i64, String)>,
) -> anyhow::Result<(Core, Option<Start>)> {
    let config = config_key(hook.as_ref());
    let found = match candidates(store.conn(), &config) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "cannot read the checkpoints; replaying the whole log");
            Vec::new()
        }
    };
    let valid = first_valid(store.conn(), &found, refused);
    let (core, start) = starting_core(store, hook, valid)?;
    if let Some(s) = start {
        twr_core::trace::mark_with(|| {
            format!("hub: checkpoint {} loaded (gseq {})", s.id, core.gseq)
        });
    }
    Ok((core, start))
}

/// What [`write`] recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Written {
    /// The new checkpoint's id.
    pub(super) id: i64,
    /// The checkpoint it started from, if any.
    pub(super) from: Option<i64>,
    /// The control position it covers up to.
    pub(super) gseq: u64,
    /// How many events it folded past where it started.
    pub(super) events: usize,
    /// The size of its state, in bytes.
    pub(super) bytes: usize,
}

/// Writes a checkpoint of the log at `db` (a database a hub opened) when `min_events` or more
/// events came after its newest checkpoint a start would use (or are in the log, without one):
/// a replay on connections of its own, from that checkpoint or from nothing, with the hook
/// policy of `notify`, up to the log's last control event. `None`: none was due, or there is no
/// control event to cover. A payload that cannot be hydrated fails the write (a start folds
/// its preview instead; a checkpoint must not keep that).
pub(super) fn write(
    db: &Path,
    notify: &NotifyConfig,
    min_events: u64,
) -> anyhow::Result<Option<Written>> {
    write_upto(db, notify, min_events, None)
}

/// [`write`], covering the log at most up to event `upto` (tests and the bench: a checkpoint
/// anywhere).
pub(super) fn write_upto(
    db: &Path,
    notify: &NotifyConfig,
    min_events: u64,
    upto: Option<&str>,
) -> anyhow::Result<Option<Written>> {
    let build = build_key();
    let hook = hook::HookFeed::new(notify).map(|(feed, _)| feed);
    let config = config_key(hook.as_ref());
    let store = Store::open_existing(db)?;
    let found = candidates(store.conn(), &config)?;
    let valid = first_valid(store.conn(), &found, &mut Vec::new());
    let after = valid.as_ref().map(|(s, _)| s.row);
    if store::events_after(store.conn(), after)? < min_events.max(1) {
        return Ok(None);
    }
    let (mut core, start) = starting_core(store, hook, valid)?;
    let mut events = store::load_with_gseq(core.store.conn(), start.map(|s| s.row))?;
    if let Some(id) = upto {
        let Some(i) = events.iter().position(|(ev, _)| ev.id == id) else {
            return Ok(None);
        };
        events.truncate(i + 1);
    }
    // Up to the last control event: one that stays in the log.
    let Some(last) = events.iter().rposition(|(_, g)| g.is_some()) else {
        return Ok(None);
    };
    core.fold_events_strict(&events[..=last])?;
    let last_event = events[last].0.id.clone();
    let gseq = core.gseq;
    drop(events);
    let (store, snap) = core.into_snapshot();
    let state = snap.to_bytes()?;
    drop(snap);
    let id = store::insert_checkpoint(
        store.conn(),
        &NewCheckpoint {
            format: FORMAT,
            build: &build,
            config: &config,
            gseq,
            last_event: &last_event,
            checksum: &checksum(FORMAT, &build, &config, gseq, &last_event, &state),
            state: &state,
        },
    )?;
    Ok(Some(Written {
        id,
        from: start.map(|s| s.id),
        gseq,
        events: last + 1,
        bytes: state.len(),
    }))
}

/// Writes a checkpoint when one is due ([`write`]): first soon after the hub opened, then every
/// `every`, until the hub stops.
pub(super) async fn checkpoint_ticker(hub: Weak<Inner>, every: Duration, min_events: u64) {
    let mut wait = every.min(FIRST_LOOK);
    loop {
        tokio::time::sleep(wait).await;
        wait = every;
        let (db, notify) = {
            let Some(inner) = hub.upgrade() else {
                return;
            };
            if *inner.state.borrow() != HubState::Running {
                return;
            }
            (inner.clone(), inner.config.notify.clone())
        };
        let t = std::time::Instant::now();
        // Never while the log is compacted (`hub::prune`: a `VACUUM` renumbers its rows).
        let done = tokio::task::spawn_blocking(move || {
            let _one = db
                .maintenance
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            write(&db.db_path, &notify, min_events)
        })
        .await;
        match done {
            Ok(Ok(Some(w))) => {
                tracing::info!(
                    checkpoint = w.id,
                    gseq = w.gseq,
                    events = w.events,
                    bytes = w.bytes,
                    ms = t.elapsed().as_millis() as u64,
                    "wrote a checkpoint"
                );
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"), "cannot write a checkpoint");
            }
            Err(e) => tracing::warn!(error = %e, "the checkpoint task failed"),
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use serde_json::{json, Value};
    use twr_core::config::{Config, NotifyConfig};
    use twr_core::event::*;
    use twr_core::notify::NotifyEvent;
    use twr_core::testkit::ThreadScript;

    use super::*;
    use crate::hub::{Hub, HubOptions};
    use crate::store::{AppendBatch, StagedBlobs};

    /// The hook feed `notify` gives the hub (`None` without a command).
    fn feed(notify: &NotifyConfig) -> Option<hook::HookFeed> {
        hook::HookFeed::new(notify).map(|(feed, _)| feed)
    }

    /// `[notify]` with a command and every event: the hook's policy is part of the fold.
    fn with_hook() -> NotifyConfig {
        NotifyConfig {
            command: vec!["true".into()],
            events: vec![
                NotifyEvent::Permission,
                NotifyEvent::Question,
                NotifyEvent::Confirm,
                NotifyEvent::Error,
                NotifyEvent::Review,
                NotifyEvent::Hung,
                NotifyEvent::Stalled,
                NotifyEvent::Report,
                NotifyEvent::Reply,
            ],
            ..Default::default()
        }
    }

    /// The folded state of `core`, as a checkpoint holds it: what the tests compare.
    pub(in crate::hub) fn fold_dump(core: Core) -> Vec<u8> {
        core.into_snapshot().1.to_bytes().unwrap()
    }

    /// Where JSON `a` and `b` first differ (`None`: they do not).
    pub(in crate::hub) fn first_difference(a: &Value, b: &Value, path: &str) -> Option<String> {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                for k in x.keys().chain(y.keys()) {
                    let (p, q) = (x.get(k), y.get(k));
                    let at = format!("{path}.{k}");
                    match (p, q) {
                        (Some(p), Some(q)) => {
                            if let Some(d) = first_difference(p, q, &at) {
                                return Some(d);
                            }
                        }
                        _ => return Some(format!("{at}: {p:?} vs {q:?}")),
                    }
                }
                None
            }
            (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
                .iter()
                .zip(y)
                .enumerate()
                .find_map(|(i, (p, q))| first_difference(p, q, &format!("{path}[{i}]"))),
            _ if a == b => None,
            _ => {
                let cut = |v: &Value| v.to_string().chars().take(300).collect::<String>();
                Some(format!("{path}: {} vs {}", cut(a), cut(b)))
            }
        }
    }

    /// Why dumps `a` and `b` differ.
    pub(in crate::hub) fn diff(a: &[u8], b: &[u8]) -> String {
        let (a, b): (Value, Value) = match (serde_json::from_slice(a), serde_json::from_slice(b)) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return "not JSON".into(),
        };
        first_difference(&a, &b, "").unwrap_or_else(|| "same JSON, other bytes".into())
    }

    /// The ids of the control events of the log at `db`, in order.
    pub(in crate::hub) fn control_ids(db: &Path) -> Vec<String> {
        let conn = store::connect(db).unwrap();
        store::load_with_gseq(&conn, None)
            .unwrap()
            .into_iter()
            .filter(|(_, g)| g.is_some())
            .map(|(ev, _)| ev.id)
            .collect()
    }

    /// Writes a checkpoint of the log at `db` covering it up to control event `upto` (it may
    /// start from an earlier checkpoint, as [`write`] does).
    pub(in crate::hub) fn write_at(
        db: &Path,
        notify: &NotifyConfig,
        upto: &str,
    ) -> anyhow::Result<Option<Written>> {
        write_upto(db, notify, 1, Some(upto))
    }

    /// What a fold set of what checkpoints leave out of the fields they hold (see [`Core`]).
    fn skipped_set(core: &Core) -> Vec<String> {
        let mut out: Vec<String> = core
            .rt
            .iter()
            .flat_map(|(t, rt)| {
                rt.skipped_set()
                    .into_iter()
                    .map(move |f| format!("rt.{t}.{f}"))
            })
            .collect();
        let rest = [
            core.orch.skipped_set(),
            core.disk.skipped_set(),
            core.wake.skipped_set(),
        ];
        out.extend(rest.into_iter().flatten().map(str::to_string));
        if core.after.has_channel() {
            out.push("after.tx".into());
        }
        // What checkpoints hold of the mixed structs is what their lists say: a field skipped
        // there is one the probes above look at.
        let keys = |v: Value| -> Vec<String> {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let listed = |l: &[&str]| -> Vec<String> {
            let mut k: Vec<String> = l.iter().map(|s| s.to_string()).collect();
            k.sort();
            k
        };
        let rt = keys(serde_json::to_value(ThreadRt::default()).unwrap());
        if rt != listed(ThreadRt::KEPT) {
            out.push(format!("ThreadRt holds {rt:?}, not {:?}", ThreadRt::KEPT));
        }
        let orch = keys(serde_json::to_value(orch::OrchState::default()).unwrap());
        if orch != listed(orch::OrchState::KEPT) {
            out.push(format!(
                "OrchState holds {orch:?}, not {:?}",
                orch::OrchState::KEPT
            ));
        }
        out
    }

    /// The fields of `a` and `b` that differ by their own `PartialEq` (not through the
    /// checkpoint's serialization, which a field it leaves out passes). Names every field.
    fn differing(a: &FoldSnapshot, b: &FoldSnapshot) -> Vec<&'static str> {
        let FoldSnapshot {
            registry,
            attention,
            usage,
            seqs,
            last_ts,
            gseq,
            known_attn,
            adapter_attn,
            rt,
            orch,
            hub_questions,
            hub_cards,
            delete_cards,
            disk,
            hook,
            after,
            card_outcomes,
            context_hashes,
            project_contexts,
            hub_errors,
            stalled,
            wake,
            hung,
            went_on,
            activity,
            trash,
            deleted_rt,
            items_deleted,
            items_restored,
        } = a;
        let mut out = Vec::new();
        if *registry != b.registry {
            out.push("registry");
        }
        if *attention != b.attention {
            out.push("attention");
        }
        if *usage != b.usage {
            out.push("usage");
        }
        if *seqs != b.seqs {
            out.push("seqs");
        }
        if *last_ts != b.last_ts {
            out.push("last_ts");
        }
        if *gseq != b.gseq {
            out.push("gseq");
        }
        if *known_attn != b.known_attn {
            out.push("known_attn");
        }
        if *adapter_attn != b.adapter_attn {
            out.push("adapter_attn");
        }
        if *rt != b.rt {
            out.push("rt");
        }
        if *orch != b.orch {
            out.push("orch");
        }
        if *hub_questions != b.hub_questions {
            out.push("hub_questions");
        }
        if *hub_cards != b.hub_cards {
            out.push("hub_cards");
        }
        if *delete_cards != b.delete_cards {
            out.push("delete_cards");
        }
        if *disk != b.disk {
            out.push("disk");
        }
        if *hook != b.hook {
            out.push("hook");
        }
        if *after != b.after {
            out.push("after");
        }
        if *card_outcomes != b.card_outcomes {
            out.push("card_outcomes");
        }
        if *context_hashes != b.context_hashes {
            out.push("context_hashes");
        }
        if *project_contexts != b.project_contexts {
            out.push("project_contexts");
        }
        if *hub_errors != b.hub_errors {
            out.push("hub_errors");
        }
        if *stalled != b.stalled {
            out.push("stalled");
        }
        if *wake != b.wake {
            out.push("wake");
        }
        if *hung != b.hung {
            out.push("hung");
        }
        if *went_on != b.went_on {
            out.push("went_on");
        }
        if *activity != b.activity {
            out.push("activity");
        }
        if *trash != b.trash {
            out.push("trash");
        }
        if *deleted_rt != b.deleted_rt {
            out.push("deleted_rt");
        }
        if *items_deleted != b.items_deleted {
            out.push("items_deleted");
        }
        if *items_restored != b.items_restored {
            out.push("items_restored");
        }
        out
    }

    /// What a fold built, seen without the checkpoint's serialization: the reducers' own values
    /// (compared whole, every field, by their `PartialEq`), and what the hub answers of each
    /// thread and of the orchestrator.
    struct Views {
        registry: Registry,
        attention: AttentionStack,
        usage: UsageView,
        answers: Value,
    }

    impl Views {
        fn of(core: &Core) -> Views {
            let threads: Vec<Value> = core
                .registry
                .list()
                .into_iter()
                .map(|v| core.thread_json(v))
                .collect();
            Views {
                registry: core.registry.clone(),
                attention: core.attention.clone(),
                usage: core.usage.clone(),
                answers: json!({
                    "threads": threads,
                    "watches": core.watches_json(),
                    "carried": serde_json::to_value(core.carried_notices(false)).unwrap(),
                    "gseq": core.gseq,
                }),
            }
        }

        /// How `self` differs from `expected`, if it does.
        fn difference(&self, expected: &Views) -> Option<String> {
            let cut = |s: String| s.chars().take(2000).collect::<String>();
            if self.registry != expected.registry {
                return Some(cut(format!(
                    "registry: {:?}\nexpected {:?}",
                    self.registry, expected.registry
                )));
            }
            if self.attention != expected.attention {
                return Some(cut(format!(
                    "attention: {:?}\nexpected {:?}",
                    self.attention, expected.attention
                )));
            }
            if self.usage != expected.usage {
                return Some(format!("usage: {:?} vs {:?}", self.usage, expected.usage));
            }
            first_difference(&self.answers, &expected.answers, "answers")
        }
    }

    /// The property, on the log at `db` with `notify`'s hook: for each control event `upto` of
    /// `positions` (in log order), a hub start from a checkpoint covering the log up to it
    /// folds the state a fold of the whole log gives, byte for byte. Each checkpoint is written
    /// from the one before (the writer's way, after the first), then each from nothing; each
    /// one's state reads back as itself. Returns the states written. The log's checkpoints are
    /// dropped before and after.
    pub(in crate::hub) fn check_log(
        db: &Path,
        notify: &NotifyConfig,
        positions: &[String],
    ) -> Result<Vec<Value>, String> {
        let e = |e: anyhow::Error| format!("{e:#}");
        let core = Core::replay_full(db, feed(notify)).map_err(e)?.0;
        let set = skipped_set(&core);
        if !set.is_empty() {
            return Err(format!("checkpoints skip what the fold sets: {set:?}"));
        }
        let full_views = Views::of(&core);
        let full_snap = core.into_snapshot().1;
        let full = full_snap.to_bytes().map_err(e)?;
        let conn = store::connect(db).map_err(e)?;
        store::drop_checkpoints(&conn).map_err(e)?;
        let mut states = Vec::new();
        for scratch in [false, true] {
            let mut previous = None;
            for upto in positions {
                if scratch {
                    store::drop_checkpoints(&conn).map_err(e)?;
                }
                let w = write_at(db, notify, upto)
                    .map_err(e)?
                    .ok_or_else(|| format!("no checkpoint written up to {upto}"))?;
                if w.from != previous {
                    return Err(format!(
                        "the checkpoint up to {upto} started from {:?}, not {previous:?}",
                        w.from
                    ));
                }
                if !scratch {
                    previous = Some(w.id);
                }
                let bytes = store::checkpoint_state(&conn, w.id).map_err(e)?.unwrap();
                let again = FoldSnapshot::from_bytes(&bytes)
                    .and_then(|s| s.to_bytes())
                    .map_err(e)?;
                if again != bytes {
                    return Err(format!(
                        "the checkpoint up to {upto} does not read back as itself: {}",
                        diff(&bytes, &again)
                    ));
                }
                states.push(serde_json::from_slice(&bytes).map_err(|e| e.to_string())?);
                let (core, _) = Core::replay_with(db, feed(notify)).map_err(e)?;
                if core.replayed.checkpoint != Some(w.id) {
                    return Err(format!(
                        "the start did not use the checkpoint up to {upto}: {:?}",
                        core.replayed
                    ));
                }
                if let Some(d) = Views::of(&core).difference(&full_views) {
                    return Err(format!("from the checkpoint up to {upto}: {d}"));
                }
                let snap = core.into_snapshot().1;
                let fields = differing(&snap, &full_snap);
                if !fields.is_empty() {
                    return Err(format!(
                        "from the checkpoint up to {upto}, the fold differs in {fields:?}"
                    ));
                }
                let got = snap.to_bytes().map_err(e)?;
                if got != full {
                    return Err(format!(
                        "from the checkpoint up to {upto} ({} events folded past it): {}",
                        w.events,
                        diff(&got, &full)
                    ));
                }
            }
        }
        store::drop_checkpoints(&conn).map_err(e)?;
        Ok(states)
    }

    /// `ext` with `ext.twr = twr`.
    pub(in crate::hub) fn twr(twr: Value) -> Option<JsonMap> {
        json!({ "twr": twr }).as_object().cloned()
    }

    /// A log made up event by event, each thread's numbered by a [`ThreadScript`].
    #[derive(Default)]
    pub(in crate::hub) struct Log {
        scripts: HashMap<String, ThreadScript>,
        pub(in crate::hub) events: Vec<Event>,
        clock: i64,
    }

    impl Log {
        /// Adds the event `f` makes with `thread`'s script, from `source`.
        pub(in crate::hub) fn with(
            &mut self,
            thread: &str,
            source: Source,
            f: impl FnOnce(&mut ThreadScript) -> Event,
        ) -> Event {
            let s = self
                .scripts
                .entry(thread.to_string())
                .or_insert_with(|| ThreadScript::new(thread));
            s.at(self.clock);
            self.clock += 7;
            let mut ev = f(s);
            ev.source = source;
            self.events.push(ev.clone());
            ev
        }

        /// Adds `body` on `thread`, from `source`.
        pub(in crate::hub) fn ev(
            &mut self,
            thread: &str,
            source: Source,
            body: EventBody,
        ) -> Event {
            self.with(thread, source, |s| s.event(body))
        }

        /// The last event added, changed by `f`.
        fn edit(&mut self, f: impl FnOnce(&mut EventBody)) -> Event {
            let ev = self.events.last_mut().unwrap();
            f(&mut ev.body);
            ev.clone()
        }

        /// Records the events in a new log at `db`, a capped payload's blobs with it.
        pub(in crate::hub) fn write(&self, db: &Path) {
            let mut store = Store::open(db).unwrap();
            for ev in &self.events {
                let staged = StagedBlobs::default();
                let capped = Event {
                    body: twr_core::blob::cap_body(&ev.body, &staged).unwrap(),
                    ..ev.clone()
                };
                let blobs = staged.take();
                store
                    .append_batch(AppendBatch {
                        events: vec![(&capped, capped.is_control())],
                        blobs: &blobs,
                        ..Default::default()
                    })
                    .unwrap();
            }
        }
    }

    pub(in crate::hub) const USER: fn() -> Source = || Source::User("tui".into());
    pub(in crate::hub) const ADAPTER: fn() -> Source = || Source::Adapter("fake".into());

    /// A log that goes through what the hub folds: an orchestrator and its user and notice
    /// turns, a watch firing, a message taken back, workers with turns, requests, background
    /// tasks, interim reports, a wake, usage costs, a context compaction, stalls and a hung-turn
    /// card, a crash, hub
    /// questions, confirm, delete, disk and after cards, prompts waiting for other threads, a
    /// deletion to the trash and its restore, a rename, context and account usage, and capped
    /// payloads. With `quiet_end`, every thread ends stopped and idle, nothing waits, and the
    /// last turn is reported.
    pub(in crate::hub) fn rich_log(db: &Path, quiet_end: bool) {
        let mut l = Log::default();
        let hub = Source::Hub;
        // The orchestrator and three workers.
        l.ev(
            "t-o",
            hub.clone(),
            EventBody::ThreadCreated(ThreadCreated {
                name: "orch".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "dontAsk".into(),
                ext: twr(json!({"role": "orchestrator"})),
                ..Default::default()
            }),
        );
        l.with("t-o", ADAPTER(), |s| s.started("so"));
        for w in ["w1", "w2", "w3"] {
            let id = format!("t-{w}");
            l.ev(
                &id,
                hub.clone(),
                EventBody::ThreadCreated(ThreadCreated {
                    name: w.into(),
                    project: Some("proj".into()),
                    box_name: "local".into(),
                    runtime: "fake".into(),
                    mode: "manual".into(),
                    ext: twr(json!({"worktree": "twr", "branch": format!("twr/{w}")})),
                    ..Default::default()
                }),
            );
            l.with(&id, ADAPTER(), |s| s.started(&format!("s-{w}")));
            l.edit(|b| {
                if let EventBody::ThreadStarted(p) = b {
                    p.ext = twr(json!({"pid": 4242, "start_time": 17, "exe": "/bin/fake"}));
                }
            });
        }
        // A watch on w1.
        l.ev(
            "t-o",
            hub.clone(),
            EventBody::ConversationUpdate(ConversationUpdate {
                text: "watching w1".into(),
                ext: twr(
                    json!({"watch": {"id": "w1", "thread": "t-w1", "note": "tell me",
                    "repeat": false}}),
                ),
                ..Default::default()
            }),
        );
        // The user's message to the orchestrator, and its turn.
        let arrived = l.ev(
            HUB_THREAD,
            USER(),
            EventBody::DecisionMade(DecisionMade {
                site: DecisionSite::Route,
                provider: DecisionProvider::User,
                options: vec!["orchestrator".into()],
                chosen: "orchestrator".into(),
                confidence: 1.0,
                ext: twr(json!({"user_prompt_for": "t-o"})),
                ..Default::default()
            }),
        );
        l.with("t-o", USER(), |s| s.turn_started("please run w1"));
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::User;
                p.routed_by = Some(arrived.id.clone());
                p.ext = twr(json!({"said_after": 2}));
            }
        });
        l.ev(
            "t-w1",
            Source::Orchestrator,
            EventBody::ThreadModeChanged(ThreadModeChanged {
                mode: "auto".into(),
                previous: "manual".into(),
                by: ChangedBy::Orchestrator,
                ..Default::default()
            }),
        );
        l.ev(
            "t-w2",
            USER(),
            EventBody::ThreadModeChanged(ThreadModeChanged {
                mode: "manual".into(),
                previous: "manual".into(),
                by: ChangedBy::User,
                ..Default::default()
            }),
        );
        // w1 works: text, a tool call too long for the log, a request answered, a background
        // task, an interim report, the task's end (the watch's held line, the wake's tasks).
        l.with("t-w1", Source::Orchestrator, |s| s.turn_started("run it"));
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::Orchestrator;
            }
        });
        l.with("t-w1", ADAPTER(), |s| s.text("Looking."));
        l.ev(
            "t-w1",
            ADAPTER(),
            EventBody::AgentThinking(AgentThinking {
                text: "hmm".into(),
                ..Default::default()
            }),
        );
        l.ev(
            "t-w1",
            ADAPTER(),
            EventBody::AgentToolCall(AgentToolCall {
                call_id: "c1".into(),
                tool: "Bash".into(),
                input_summary: format!("cargo test {}", "x".repeat(6000)),
                ..Default::default()
            }),
        );
        l.with("t-w1", ADAPTER(), |s| {
            s.raise("t-w1:a1", AttentionReason::Permission)
        });
        l.with("t-w1", USER(), |s| s.resolve("t-w1:a1"));
        l.ev(
            "t-w1",
            ADAPTER(),
            EventBody::TaskUpdated(TaskUpdated {
                task_id: "bg1".into(),
                running: true,
                description: "build".into(),
                ..Default::default()
            }),
        );
        l.with("t-w1", ADAPTER(), |s| s.text("Started the build."));
        l.with("t-w1", ADAPTER(), |s| s.turn_ended(TurnResult::Completed));
        l.edit(|b| {
            if let EventBody::TurnEnded(p) = b {
                p.usage = Some(Usage {
                    input: 10,
                    output: 5,
                    cost: Some(0.1),
                });
            }
        });
        l.with("t-w1", hub.clone(), |s| s.report(Some(1), Grade::Success));
        l.edit(|b| {
            if let EventBody::ReportReady(p) = b {
                p.ext = twr(json!({"background_tasks": 1}));
            }
        });
        l.ev(
            "t-w1",
            ADAPTER(),
            EventBody::TaskUpdated(TaskUpdated {
                task_id: "bg1".into(),
                running: false,
                description: "build: done".into(),
                ..Default::default()
            }),
        );
        l.ev(
            "t-w1",
            hub.clone(),
            EventBody::ConversationUpdate(ConversationUpdate {
                text: "woke w1".into(),
                ext: twr(json!({"auto_wake": {"turn": 1, "tasks": 1}})),
                ..Default::default()
            }),
        );
        l.with("t-w1", hub.clone(), |s| {
            s.turn_started("[twr] Your background work finished")
        });
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::Runtime;
            }
        });
        l.with("t-w1", ADAPTER(), |s| s.text("All good."));
        l.with("t-w1", ADAPTER(), |s| s.turn_ended(TurnResult::Completed));
        l.edit(|b| {
            if let EventBody::TurnEnded(p) = b {
                p.usage = Some(Usage {
                    input: 3,
                    output: 4,
                    cost: Some(0.2),
                });
            }
        });
        l.with("t-w1", hub.clone(), |s| s.report(Some(2), Grade::Success));
        l.with("t-o", ADAPTER(), |s| s.text("Done."));
        l.with("t-o", ADAPTER(), |s| s.turn_ended(TurnResult::Completed));
        // A second message, taken back, and a turn of the runtime's own after it.
        let arrived = l.ev(
            HUB_THREAD,
            USER(),
            EventBody::DecisionMade(DecisionMade {
                site: DecisionSite::Route,
                provider: DecisionProvider::User,
                options: vec!["orchestrator".into()],
                chosen: "orchestrator".into(),
                confidence: 1.0,
                ext: twr(json!({"user_prompt_for": "t-o"})),
                ..Default::default()
            }),
        );
        l.with("t-o", USER(), |s| s.turn_started("no, stop"));
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::User;
                p.routed_by = Some(arrived.id.clone());
            }
        });
        l.ev(
            "t-o",
            USER(),
            EventBody::TurnWithdrawn(TurnWithdrawn {
                msg_ref: 7,
                turn: 2,
                ..Default::default()
            }),
        );
        l.with("t-o", hub.clone(), |s| {
            s.turn_ended(TurnResult::Interrupted)
        });
        l.with("t-o", ADAPTER(), |s| s.turn_started(""));
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::Runtime;
            }
        });
        l.with("t-o", ADAPTER(), |s| s.turn_ended(TurnResult::Completed));
        // The user compacts the orchestrator's context (maintenance).
        l.with("t-o", USER(), |s| s.turn_started("/compact"));
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::User;
                p.ext = twr(json!({"compact": true}));
            }
        });
        l.with("t-o", ADAPTER(), |s| s.turn_ended(TurnResult::Completed));
        // Hub cards: a question routing a message, a confirm card (confirmed), a delete card
        // (answered), a disk card.
        l.with(HUB_THREAD, hub.clone(), |s| {
            s.raise("q1", AttentionReason::Question)
        });
        l.edit(|b| {
            if let EventBody::AttentionRaised(p) = b {
                p.ext = twr(json!({"pending_message": "hello", "asked_by": "user:tui",
                    "said_after": 5}));
            }
        });
        l.with(HUB_THREAD, hub.clone(), |s| {
            s.raise("c1", AttentionReason::Question)
        });
        l.edit(|b| {
            if let EventBody::AttentionRaised(p) = b {
                p.ext = twr(
                    json!({"proposal": {"orchestrator": "t-o", "turn": 1, "actions": [
                    {"kind": "answer", "attn_id": "x", "revision": 1, "thread": "t-w1",
                     "answer": "allow"}]}}),
                );
            }
        });
        l.with(HUB_THREAD, USER(), |s| s.resolve("c1"));
        l.edit(|b| {
            if let EventBody::AttentionResolved(p) = b {
                p.ext = twr(json!({"confirmed": {"applied": ["answer x"], "skipped": []}}));
            }
        });
        // A prompt for w2 waiting for w3, with its card, and one for w1 waiting for w2.
        for (id, target, after) in [("g1", "t-w2", "t-w3"), ("g2", "t-w1", "t-w2")] {
            l.ev(
                target,
                hub.clone(),
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: "queued".into(),
                    ext: twr(json!({"send_after": {"id": id, "prompt": "go on",
                        "after": [{"thread": after, "turn": 0}], "source": "user:tui",
                        "from": "user"}})),
                    ..Default::default()
                }),
            );
        }
        l.with(HUB_THREAD, hub.clone(), |s| {
            s.raise("ac1", AttentionReason::Question)
        });
        l.edit(|b| {
            if let EventBody::AttentionRaised(p) = b {
                p.ext = twr(json!({"after_card": {"id": "g1"}}));
            }
        });
        l.with(HUB_THREAD, hub.clone(), |s| {
            s.raise("d1", AttentionReason::Question)
        });
        l.edit(|b| {
            if let EventBody::AttentionRaised(p) = b {
                p.ext = twr(
                    json!({"delete": {"threads": [{"thread": "t-w3", "name": "w3",
                    "lifecycle": 1}], "asked_by": "user:tui"}}),
                );
            }
        });
        l.with(HUB_THREAD, USER(), |s| s.resolve("d1"));
        // w3 compacting its context when it is stopped.
        l.with("t-w3", USER(), |s| s.turn_started("/compact"));
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::User;
                p.ext = twr(json!({"compact": true}));
            }
        });
        l.ev(
            "t-w3",
            ADAPTER(),
            EventBody::ContextCompacting(ContextCompacting {
                trigger: Some("manual".into()),
                ..Default::default()
            }),
        );
        // w3 deleted to the trash (its waiting prompt orphaned), then restored.
        l.with("t-w3", hub.clone(), |s| s.stopped());
        l.ev(
            "t-w3",
            hub.clone(),
            EventBody::ThreadDeleted(ThreadDeleted {
                name: "w3".into(),
                trash: Some("item-1".into()),
                trash_bytes: Some(4096),
                branch: Some("twr/w3".into()),
                losses: Some("nothing".into()),
                ..Default::default()
            }),
        );
        l.ev(
            "t-w2",
            hub.clone(),
            EventBody::ConversationUpdate(ConversationUpdate {
                text: "dropped".into(),
                ext: twr(json!({"send_after_end": {"id": "g1"}})),
                ..Default::default()
            }),
        );
        l.with(HUB_THREAD, hub.clone(), |s| s.resolve("ac1"));
        l.ev(
            "t-w3",
            USER(),
            EventBody::ThreadRestored(ThreadRestored {
                item: "item-1".into(),
                name: "w3".into(),
                branch: Some("twr/w3".into()),
                ..Default::default()
            }),
        );
        l.with(HUB_THREAD, hub.clone(), |s| {
            s.raise("k1", AttentionReason::Question)
        });
        l.edit(|b| {
            if let EventBody::AttentionRaised(p) = b {
                p.ext = twr(json!({"disk": {"box": "local",
                    "threads": [{"thread": "t-w1", "lifecycle": 1}]}}));
            }
        });
        // w2: a turn that stalls (and goes on), a hung-turn card the user answers, an error,
        // a crash that takes the turn down, a failed report.
        l.with("t-w2", Source::Orchestrator, |s| s.turn_started("fix it"));
        l.ev(
            "t-w2",
            ADAPTER(),
            EventBody::ContextCompacting(ContextCompacting {
                trigger: Some("auto".into()),
                ..Default::default()
            }),
        );
        l.ev(
            "t-w2",
            ADAPTER(),
            EventBody::ContextCompacted(ContextCompacted {
                trigger: "auto".into(),
                pre_tokens: Some(150_000),
                post_tokens: Some(20_000),
                ..Default::default()
            }),
        );
        l.with("t-w2", hub.clone(), |s| {
            s.raise("s1", AttentionReason::Stalled)
        });
        l.with("t-w2", hub.clone(), |s| s.resolve("s1"));
        l.edit(|b| {
            if let EventBody::AttentionResolved(p) = b {
                p.by = ResolvedBy::Timeout;
                p.answer = None;
                p.ext = twr(json!({"went_on": true}));
            }
        });
        l.with("t-w2", hub.clone(), |s| {
            s.raise("h1", AttentionReason::Stalled)
        });
        l.edit(|b| {
            if let EventBody::AttentionRaised(p) = b {
                p.ext = twr(json!({"hung": {"turn": 1, "waiting": "a prompt"}}));
            }
        });
        l.with("t-w2", USER(), |s| s.resolve("h1"));
        l.with("t-w2", hub.clone(), |s| {
            s.raise("e1", AttentionReason::Error)
        });
        l.with("t-w2", ADAPTER(), |s| s.crashed(1));
        l.with("t-w2", hub.clone(), |s| s.report(Some(1), Grade::GaveUp));
        l.with("t-w2", USER(), |s| s.resumed("s-w2"));
        l.with("t-w2", hub.clone(), |s| s.resolve("e1"));
        // w3 compacts its context in a turn of its own while a task of its ends.
        l.with("t-w3", hub.clone(), |s| s.turn_started(""));
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::User;
                p.ext = twr(json!({ COMPACT_TURN_KEY: true }));
            }
        });
        l.ev(
            "t-w3",
            ADAPTER(),
            EventBody::TaskUpdated(TaskUpdated {
                task_id: "bg3".into(),
                running: false,
                description: "done".into(),
                ..Default::default()
            }),
        );
        l.with("t-w3", ADAPTER(), |s| s.turn_ended(TurnResult::Completed));
        // A rename, the standing context, an account's usage, a notice turn.
        l.ev(
            "t-w1",
            USER(),
            EventBody::ThreadRenamed(ThreadRenamed {
                name: "w1b".into(),
                previous: "w1".into(),
                by: ChangedBy::User,
                ..Default::default()
            }),
        );
        l.ev(
            HUB_THREAD,
            hub.clone(),
            EventBody::ContextUpdated(ContextUpdated {
                hash: "sha256:abc".into(),
                ext: twr(json!({"path": "/repo/CONTEXT.md", "project": "proj"})),
                ..Default::default()
            }),
        );
        l.ev(
            HUB_THREAD,
            hub.clone(),
            EventBody::AccountUsage(AccountUsage {
                runtime: "claude-code".into(),
                windows: vec![twr_core::usage::UsageWindow {
                    window: "5h".into(),
                    used_pct: 12.5,
                    resets_at: None,
                    minutes: Some(300),
                    observed_at: None,
                }],
                source: "event".into(),
                ..Default::default()
            }),
        );
        l.with("t-o", hub.clone(), |s| s.turn_started("[twr] notices"));
        l.edit(|b| {
            if let EventBody::TurnStarted(p) = b {
                p.from = TurnFrom::User;
                p.ext = twr(json!({"notice": true, "watches_fired": ["w1"]}));
            }
        });
        l.with("t-o", ADAPTER(), |s| s.turn_ended(TurnResult::Completed));
        // A user's message the orchestrator is working on when the log ends.
        l.with("t-w1", ADAPTER(), |s| s.turn_started("more"));
        l.with("t-w1", ADAPTER(), |s| s.text(&"long text ".repeat(800)));
        l.edit(|b| {
            if let EventBody::AgentText(t) = b {
                t.extra.insert("detail".into(), json!("x".repeat(5000)));
            }
        });
        if quiet_end {
            l.with("t-w1", ADAPTER(), |s| s.turn_ended(TurnResult::Completed));
            l.with("t-w1", hub.clone(), |s| s.report(Some(3), Grade::Success));
            l.ev(
                "t-w1",
                hub.clone(),
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: "ran".into(),
                    ext: twr(json!({"send_after_end": {"id": "g2"}})),
                    ..Default::default()
                }),
            );
            l.with(HUB_THREAD, USER(), |s| s.resolve("k1"));
            for t in ["t-o", "t-w1", "t-w2", "t-w3"] {
                l.with(t, hub.clone(), |s| s.stopped());
            }
        }
        l.write(db);
    }

    /// Checkpoint `meta`'s row as it is, with state `state`, changed by `change`.
    fn insert_like(
        db: &Path,
        meta: &CheckpointMeta,
        state: &[u8],
        change: impl FnOnce(&mut CheckpointMeta),
    ) -> i64 {
        let mut m = meta.clone();
        change(&mut m);
        m.checksum = checksum(m.format, &m.build, &m.config, m.gseq, &m.last_event, state);
        insert_raw(db, &m, state)
    }

    /// Checkpoint `meta`'s row as it is (its checksum too), with state `state`.
    fn insert_raw(db: &Path, m: &CheckpointMeta, state: &[u8]) -> i64 {
        let conn = store::connect(db).unwrap();
        store::insert_checkpoint(
            &conn,
            &NewCheckpoint {
                format: m.format,
                build: &m.build,
                config: &m.config,
                gseq: m.gseq,
                last_event: &m.last_event,
                checksum: &m.checksum,
                state,
            },
        )
        .unwrap()
    }

    /// `state` with a thread no log has, and at control position `gseq` when given: a hub
    /// that started from it would list that thread.
    fn poisoned(state: &[u8], gseq: Option<u64>) -> Vec<u8> {
        let mut v: Value = serde_json::from_slice(state).unwrap();
        let threads = v["registry"]["threads"].as_object_mut().unwrap();
        let mut ghost = threads.values().next().unwrap().clone();
        ghost["id"] = json!("ghost");
        ghost["name"] = json!("ghost");
        threads.insert("ghost".into(), ghost);
        if let Some(g) = gseq {
            v["gseq"] = json!(g);
        }
        serde_json::to_vec(&v).unwrap()
    }

    /// The state a hub start rebuilds from `db` (with no hook), and how.
    fn started(db: &Path) -> (Vec<u8>, Replayed) {
        let (core, _) = Core::replay_with(db, None).unwrap();
        let replayed = core.replayed.clone();
        (fold_dump(core), replayed)
    }

    /// A checkpoint of another format, build or settings, one whose last event the log does
    /// not have (or has at another control position), and one whose state does not read are
    /// each passed over, with why, for an older one or the whole log; the state is then the
    /// whole log's. (Each holds a thread no log has: had it been used, it would show.)
    #[test]
    fn a_checkpoint_it_cannot_use_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, false);
        let notify = NotifyConfig::default();
        let full = fold_dump(Core::replay_full(&db, None).unwrap().0);
        let events = control_ids(&db).len();
        // None: the whole log.
        let (state, replayed) = started(&db);
        assert_eq!(replayed.checkpoint, None);
        assert!(replayed.refused.is_empty() && replayed.events > events);
        assert!(state == full, "{}", diff(&state, &full));
        // A usable one, to copy.
        let w = write(&db, &notify, 1).unwrap().unwrap();
        let conn = store::connect(&db).unwrap();
        let meta = store::checkpoints(&conn).unwrap().remove(0);
        assert_eq!(meta.id, w.id);
        let good = store::checkpoint_state(&conn, w.id).unwrap().unwrap();
        let poison = poisoned(&good, None);
        // (A poisoned one that is used shows.)
        store::drop_checkpoints(&conn).unwrap();
        let id = insert_like(&db, &meta, &poison, |_| {});
        let (state, replayed) = started(&db);
        assert_eq!(replayed.checkpoint, Some(id));
        assert!(state != full);
        let other_settings = config_key(feed(&with_hook()).as_ref());
        let other_gseq = poisoned(&good, Some(meta.gseq + 1));
        type Change = Box<dyn FnOnce(&mut CheckpointMeta)>;
        let cases: Vec<(&str, Change, &[u8])> = vec![
            ("format 2, not 1", Box::new(|m| m.format = 2), &poison[..]),
            (
                "written by 0.0.0 0",
                Box::new(|m| m.build = "0.0.0 0".into()),
                &poison[..],
            ),
            (
                "written with other settings",
                Box::new(move |m| m.config = other_settings),
                &poison[..],
            ),
            (
                "its last event nope is not in the log",
                Box::new(|m| m.last_event = "nope".into()),
                &poison[..],
            ),
            (
                "is at control position",
                Box::new(|m| m.gseq += 1),
                &poison[..],
            ),
            (
                "its state does not read",
                Box::new(|_| {}),
                &b"{\"registry\""[..],
            ),
            ("its state does not read", Box::new(|_| {}), &other_gseq[..]),
        ];
        for (why, change, state) in cases {
            store::drop_checkpoints(&conn).unwrap();
            let id = insert_like(&db, &meta, state, change);
            let (got, replayed) = started(&db);
            assert_eq!(replayed.checkpoint, None, "{why}");
            assert_eq!(replayed.refused.len(), 1, "{why}: {:?}", replayed.refused);
            assert_eq!(replayed.refused[0].0, id);
            assert!(
                replayed.refused[0].1.contains(why),
                "{why}: {:?}",
                replayed.refused
            );
            assert!(got == full, "{why}: {}", diff(&got, &full));
        }
        // A state changed since it was written that still reads (a thread's last seq one less:
        // its next append would collide), with the checksum it was written with.
        let mut corrupt: Value = serde_json::from_slice(&good).unwrap();
        let seq = corrupt["seqs"]["t-w2"].as_u64().unwrap();
        corrupt["seqs"]["t-w2"] = json!(seq - 1);
        let corrupt = serde_json::to_vec(&corrupt).unwrap();
        assert!(FoldSnapshot::from_bytes(&corrupt).is_ok());
        store::drop_checkpoints(&conn).unwrap();
        let id = insert_raw(&db, &meta, &corrupt);
        let (got, replayed) = started(&db);
        assert_eq!(replayed.checkpoint, None);
        assert_eq!(replayed.refused.len(), 1);
        assert_eq!(replayed.refused[0].0, id);
        assert!(
            replayed.refused[0]
                .1
                .contains("its checksum does not match"),
            "{:?}",
            replayed.refused
        );
        assert!(got == full, "{}", diff(&got, &full));
        // (With the checksum of the changed state, it is used, and shows.)
        store::drop_checkpoints(&conn).unwrap();
        insert_like(&db, &meta, &corrupt, |_| {});
        assert!(started(&db).0 != full);
        // The newest unusable, an older one usable: the older one.
        store::drop_checkpoints(&conn).unwrap();
        let older = insert_like(&db, &meta, &good, |_| {});
        let newer = insert_like(&db, &meta, b"[]", |_| {});
        let (got, replayed) = started(&db);
        assert_eq!(replayed.checkpoint, Some(older));
        assert_eq!(replayed.refused.len(), 1);
        assert_eq!(replayed.refused[0].0, newer);
        assert!(got == full, "{}", diff(&got, &full));
        // Dropped: the whole log again, the same state.
        store::drop_checkpoints(&conn).unwrap();
        let (got, replayed) = started(&db);
        assert_eq!(replayed.checkpoint, None);
        assert!(got == full);
    }

    /// A checkpoint is written only once enough events came after the newest one (or are in
    /// the log), it covers the log up to its last control event, the writer starts from the
    /// newest one, only the newest two are kept, and the log itself never changes.
    #[test]
    fn checkpoints_are_written_when_due_and_the_newest_two_kept() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, false);
        let notify = NotifyConfig::default();
        let conn = store::connect(&db).unwrap();
        let all = store::events_after(&conn, None).unwrap();
        let log = || store::load_with_gseq(&conn, None).unwrap();
        let before = log();
        assert_eq!(write(&db, &notify, all + 1).unwrap(), None);
        let first = write(&db, &notify, all).unwrap().unwrap();
        assert_eq!(first.from, None);
        // Up to the last control event: the agent text after it is not covered.
        let last_control = before.iter().rposition(|(_, g)| g.is_some()).unwrap();
        assert!(last_control + 1 < before.len());
        assert_eq!(first.events, last_control + 1);
        assert_eq!(first.gseq, store::control_head(&conn).unwrap());
        // Nothing new: none due, however few events it asks for.
        assert_eq!(write(&db, &notify, 1).unwrap(), None);
        // New events: the next one starts from the first.
        let mut l = Log {
            clock: 10_000,
            ..Default::default()
        };
        for i in 0..3 {
            l.ev(
                "t-extra",
                Source::Hub,
                EventBody::ContextUpdated(ContextUpdated {
                    hash: format!("sha256:{i}"),
                    ext: twr(json!({"project": "other"})),
                    ..Default::default()
                }),
            );
        }
        l.write(&db);
        let second = write(&db, &notify, 1).unwrap().unwrap();
        assert_eq!(second.from, Some(first.id));
        assert_eq!(
            second.events,
            1 + 3,
            "the agent text and the three new events"
        );
        let (_, replayed) = started(&db);
        assert_eq!(replayed.checkpoint, Some(second.id));
        assert_eq!(replayed.events, 0);
        // Three written: the newest two kept.
        l.ev(
            "t-extra",
            Source::Hub,
            EventBody::ContextUpdated(ContextUpdated {
                hash: "sha256:9".into(),
                ext: twr(json!({"project": "other"})),
                ..Default::default()
            }),
        );
        Log {
            events: vec![l.events.pop().unwrap()],
            ..Default::default()
        }
        .write(&db);
        let third = write(&db, &notify, 1).unwrap().unwrap();
        let kept: Vec<i64> = store::checkpoints(&conn)
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(kept, vec![third.id, second.id]);
        // The log is what it was, plus what was added here.
        let after = log();
        assert_eq!(after.len(), before.len() + 4);
        for ((a, ga), (b, gb)) in before.iter().zip(&after) {
            assert_eq!((&a.id, ga), (&b.id, gb));
        }
    }

    /// The writer counts the events from the checkpoint a start would use: past a newer one
    /// whose state was changed since (its checksum no longer matches), from the older one.
    #[test]
    fn a_checkpoint_is_due_by_the_one_a_start_would_use() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, false);
        let notify = NotifyConfig::default();
        let conn = store::connect(&db).unwrap();
        let ids = control_ids(&db);
        let older = write_at(&db, &notify, &ids[5]).unwrap().unwrap();
        let newer = write_at(&db, &notify, ids.last().unwrap())
            .unwrap()
            .unwrap();
        let metas = store::checkpoints(&conn).unwrap();
        let meta = metas.iter().find(|m| m.id == newer.id).unwrap().clone();
        let mut state: Value =
            serde_json::from_slice(&store::checkpoint_state(&conn, newer.id).unwrap().unwrap())
                .unwrap();
        state["seqs"]["t-w1"] = json!(1);
        store::drop_checkpoints(&conn).unwrap();
        let older_meta = metas.iter().find(|m| m.id == older.id).unwrap().clone();
        let older_state = {
            // (Written again: the drop took it.)
            let w = write_at(&db, &notify, &ids[5]).unwrap().unwrap();
            store::checkpoint_state(&conn, w.id).unwrap().unwrap()
        };
        store::drop_checkpoints(&conn).unwrap();
        let older_id = insert_raw(&db, &older_meta, &older_state);
        insert_raw(&db, &meta, &serde_json::to_vec(&state).unwrap());
        let after_newer = store::events_after(
            &conn,
            store::event_position(&conn, &meta.last_event)
                .unwrap()
                .map(|p| p.0),
        )
        .unwrap();
        let w = write(&db, &notify, after_newer + 1).unwrap();
        assert_eq!(w.map(|w| w.from), Some(Some(older_id)));
    }

    /// A payload the writer cannot hydrate (its blob gone) fails the write: no checkpoint keeps
    /// its preview. A start still folds the log (the preview, logged, as before).
    #[test]
    fn the_writer_fails_on_a_payload_it_cannot_hydrate() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, false);
        let conn = store::connect(&db).unwrap();
        assert!(conn.execute("DELETE FROM blobs", []).unwrap() > 0);
        let e = write(&db, &NotifyConfig::default(), 1).unwrap_err();
        assert!(format!("{e:#}").contains("hydrating event"), "{e:#}");
        assert!(store::checkpoints(&conn).unwrap().is_empty());
        let (_, replayed) = started(&db);
        assert_eq!(replayed.checkpoint, None);
    }

    /// A blob that is there but no longer matches its preview (hydration leaves the field
    /// capped, without an error) fails the write too: no checkpoint keeps the preview.
    #[test]
    fn the_writer_fails_on_a_payload_it_cannot_hydrate_whole() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, false);
        let conn = store::connect(&db).unwrap();
        let n = conn
            .execute(
                "UPDATE blobs SET bytes = CAST('something else' AS BLOB)
                 WHERE CAST(bytes AS TEXT) LIKE 'cargo test %'",
                [],
            )
            .unwrap();
        assert!(n > 0);
        let e = write(&db, &NotifyConfig::default(), 1).unwrap_err();
        assert!(format!("{e:#}").contains("stay capped"), "{e:#}");
        assert!(store::checkpoints(&conn).unwrap().is_empty());
        assert_eq!(started(&db).1.checkpoint, None);
    }

    /// Every hash set a serialized struct of twr-core or twr-hub holds is serialized sorted
    /// (`twr_core::sorted`) or skipped: a checkpoint holds many of these types, and a set
    /// serialized in its iteration order gives equal states other bytes (maps are sorted by
    /// `serde_json::Value` already). A look over the sources, since a set of one element (what
    /// a made-up log tends to hold) serializes alike in any order.
    #[test]
    fn serialized_hash_sets_are_sorted() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        for c in ["twr-core/src", "twr-hub/src"] {
            walk(&root.join(c), &mut files);
        }
        let mut unsorted = Vec::new();
        // (`sorted.rs` tests its own functions on sets it names otherwise.)
        for f in files.iter().filter(|f| !f.ends_with("sorted.rs")) {
            let text = std::fs::read_to_string(f).unwrap();
            for (line, field) in unsorted_sets(&text) {
                unsorted.push(format!("{}:{line}: {field}", f.display()));
            }
        }
        assert!(
            unsorted.is_empty(),
            "hash sets serialized unsorted:\n{}",
            unsorted.join("\n")
        );
    }

    /// The hash-set fields of the serialized structs of Rust source `text` that are neither
    /// serialized sorted nor left out whole: (line, field).
    fn unsorted_sets(text: &str) -> Vec<(usize, String)> {
        let lines: Vec<&str> = text.lines().collect();
        let mut out = Vec::new();
        // Inside a struct whose derive has `Serialize`, until its closing brace.
        let mut derives = false;
        let mut inside = false;
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            if t.starts_with("#[derive(") {
                derives = t.contains("Serialize");
                continue;
            }
            if !inside && t.contains("struct ") && t.ends_with('{') {
                inside = derives;
                derives = false;
                continue;
            }
            if inside && (t == "}" || line.starts_with('}')) {
                inside = false;
                continue;
            }
            if !inside || t.starts_with("//") || !t.contains("HashSet<") {
                continue;
            }
            // The attributes right above the field.
            let attrs: Vec<&str> = lines[..i]
                .iter()
                .rev()
                .map(|l| l.trim_start())
                .take_while(|l| l.starts_with("#[") || l.starts_with("///"))
                .filter(|l| l.starts_with("#["))
                .collect();
            let ok = attrs
                .iter()
                .any(|a| sorted_serializer(a) || skipped_whole(a));
            if !ok {
                out.push((i + 1, t.to_string()));
            }
        }
        out
    }

    /// The items of `#[serde(...)]` attribute `attr` (none for another attribute).
    fn serde_items(attr: &str) -> Vec<String> {
        attr.strip_prefix("#[serde(")
            .and_then(|a| a.strip_suffix(")]"))
            .map(|a| a.split(',').map(|i| i.trim().to_string()).collect())
            .unwrap_or_default()
    }

    /// `serialize_with` (or `with`) one of `twr_core::sorted`'s functions.
    fn sorted_serializer(attr: &str) -> bool {
        serde_items(attr).iter().any(|i| {
            let (k, v) = i.split_once('=').unwrap_or((i, ""));
            matches!(k.trim(), "serialize_with" | "with")
                && ["\"twr_core::sorted::", "\"crate::sorted::"]
                    .iter()
                    .any(|p| v.trim().starts_with(p))
        })
    }

    /// `skip` or `skip_serializing`, which leave the field out whatever it holds (not
    /// `skip_serializing_if`, which writes it when it is not empty).
    fn skipped_whole(attr: &str) -> bool {
        serde_items(attr)
            .iter()
            .any(|i| i == "skip" || i == "skip_serializing")
    }

    /// The guard's own cases: what it lets pass and what it reports.
    #[test]
    fn the_sorted_set_guard_tells_skips_apart() {
        // (Written with `SET`, so that the look over this file does not take it for code.)
        let text = r#"
#[derive(Debug, Serialize, Deserialize)]
struct S {
    #[serde(serialize_with = "twr_core::sorted::set")]
    sorted: SET<String>,
    #[serde(skip)]
    skipped: SET<String>,
    #[serde(default, skip_serializing)]
    never_written: SET<String>,
    #[serde(default, skip_serializing_if = "SET::is_empty")]
    when_not_empty: SET<String>,
    plain: SET<String>,
    #[serde(serialize_with = "my_own")]
    other: SET<String>,
}
#[derive(Debug)]
struct NotSerialized {
    any: SET<String>,
}
"#
        .replace("SET", "HashSet");
        let found: Vec<String> = unsorted_sets(&text).into_iter().map(|(_, f)| f).collect();
        assert_eq!(
            found,
            [
                "when_not_empty: HashSet<String>,",
                "plain: HashSet<String>,",
                "other: HashSet<String>,"
            ]
        );
    }

    /// Hub options for a log made up here: nothing ticks.
    pub(in crate::hub) fn quiet_options(db: &Path) -> HubOptions {
        let mut o = HubOptions::new(db, Config::default());
        o.checkpoint_tick = Some(Duration::ZERO);
        o.git_tick = Some(Duration::ZERO);
        o.trash_tick = Some(Duration::ZERO);
        o.disk_tick = Some(Duration::ZERO);
        o.stall_after = Some(Duration::ZERO);
        o.prune_tick = Some(Duration::ZERO);
        o
    }

    /// Opens a hub on `db` and shuts it down: what its start records (the reactions of turns
    /// the made-up log left without them) is in the log from then on, and the next start
    /// records nothing.
    pub(in crate::hub) async fn settle(db: &Path) {
        let hub = Hub::open(quiet_options(db)).await.unwrap();
        hub.shutdown().await;
    }

    /// What clients and the orchestrator get from `hub`: every thread (deleted ones too), the
    /// attention items, the orchestrator's watches and queued lines, the trash as the log
    /// knows it, the control head.
    pub(in crate::hub) fn answers(hub: &Hub) -> Value {
        let ids: Vec<String> = {
            let core = hub.inner.core();
            core.registry.list().iter().map(|v| v.id.clone()).collect()
        };
        let threads: serde_json::Map<String, Value> = ids
            .iter()
            .map(|id| {
                let mut j = hub.get_thread(id).unwrap();
                j.as_object_mut().unwrap().remove("live");
                (id.clone(), j)
            })
            .collect();
        let attention = hub.attention_list(true);
        let core = hub.inner.core();
        let deleted: Vec<Value> = ["t-w3"]
            .iter()
            .filter_map(|t| core.registry.deleted_view(t))
            .map(|v| serde_json::to_value(v).unwrap())
            .collect();
        json!({
            "threads": threads,
            "deleted": deleted,
            "attention": attention,
            "watches": core.watches_json(),
            "notices": serde_json::to_value(&core.orch.notices).unwrap(),
            "carried": serde_json::to_value(core.carried_notices(true)).unwrap(),
            "trash": core.trash.keys().collect::<Vec<_>>(),
            "restored": core.items_restored.keys().collect::<Vec<_>>(),
            "head": store::control_head(core.store.conn()).unwrap(),
        })
    }

    /// A hub opened on a log with a checkpoint answers clients and the orchestrator as one
    /// opened on the same log without it; a checkpoint the hub dropped changes nothing.
    #[tokio::test]
    async fn a_hub_opened_from_a_checkpoint_answers_as_one_opened_from_the_whole_log() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a/twr.db"), dir.path().join("b/twr.db"));
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        rich_log(&a, true);
        settle(&a).await;
        for ext in ["", "-wal", "-shm"] {
            let from = format!("{}{ext}", a.display());
            if std::path::Path::new(&from).exists() {
                std::fs::copy(&from, format!("{}{ext}", b.display())).unwrap();
            }
        }
        // b: a checkpoint two thirds in.
        let ids = control_ids(&b);
        let mid = &ids[ids.len() * 2 / 3];
        let w = write_at(&b, &NotifyConfig::default(), mid)
            .unwrap()
            .unwrap();
        let (ha, hb) = (
            Hub::open(quiet_options(&a)).await.unwrap(),
            Hub::open(quiet_options(&b)).await.unwrap(),
        );
        assert_eq!(ha.inner.core().replayed.checkpoint, None);
        assert_eq!(hb.inner.core().replayed.checkpoint, Some(w.id));
        let (ja, jb) = (answers(&ha), answers(&hb));
        assert!(
            ja == jb,
            "{}",
            first_difference(&ja, &jb, "").unwrap_or_default()
        );
        assert!(ja["threads"].as_object().unwrap().len() >= 4);
        assert!(!ja["attention"].as_array().unwrap().is_empty());
        ha.shutdown().await;
        hb.shutdown().await;
        drop((ha, hb));
        // Dropped: the same.
        store::drop_checkpoints(&store::connect(&b).unwrap()).unwrap();
        let hb = Hub::open(quiet_options(&b)).await.unwrap();
        assert_eq!(hb.inner.core().replayed.checkpoint, None);
        let jb = answers(&hb);
        assert!(
            ja == jb,
            "{}",
            first_difference(&ja, &jb, "").unwrap_or_default()
        );
        hb.shutdown().await;
    }

    /// A hub opened from a checkpoint has not read every event: it reads the compacted
    /// backlog's rows in the background as it opens, by itself, the rows a whole replay
    /// builds, and its compacted backlog is what the log's events select. (A subscription
    /// that comes first reads them itself: `a_reopened_hub_compacts_its_log_alike`.)
    #[tokio::test]
    async fn a_hub_opened_from_a_checkpoint_compacts_its_backlog_alike() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, true);
        settle(&db).await;
        let ids = control_ids(&db);
        let w = write_at(&db, &NotifyConfig::default(), &ids[ids.len() / 2])
            .unwrap()
            .unwrap();
        let hub = Hub::open(quiet_options(&db)).await.unwrap();
        assert_eq!(hub.inner.core().replayed.checkpoint, Some(w.id));
        let (stored, head) = hub.control_read(0, 100_000).unwrap();
        // Nobody subscribes: the hub reads them by itself.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let rows = loop {
            let rows = hub.inner.compact_rows.lock().unwrap().rows().to_vec();
            if rows.last().map(|r| r.gseq) == Some(head) || std::time::Instant::now() > deadline {
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let replayed: Vec<twr_core::compact::Row> = stored
            .iter()
            .map(|(g, e)| twr_core::compact::Row::of(*g, e))
            .collect();
        assert_eq!(rows, replayed);
        let kept = twr_core::compact::select_events(stored.iter().map(|(g, e)| (*g, e)), 0);
        let mut sub = hub.subscribe_control_with(0, true);
        assert_eq!(sub.head(), head);
        let n = tokio::time::timeout(Duration::from_secs(10), sub.compact())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, kept.len() as u64);
        let mut got = Vec::new();
        while got.len() < kept.len() {
            let next = tokio::time::timeout(Duration::from_secs(10), sub.next());
            got.push(next.await.unwrap().unwrap().unwrap().0);
        }
        assert_eq!(got, kept);
        hub.shutdown().await;
    }

    /// The hub writes a checkpoint in the background soon after it opens (here at once: one
    /// event is enough), without changing the log, and the next start folds from it.
    #[tokio::test]
    async fn the_hub_writes_a_checkpoint_and_the_next_start_folds_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, true);
        settle(&db).await;
        let conn = store::connect(&db).unwrap();
        let before = store::events_after(&conn, None).unwrap();
        let mut o = quiet_options(&db);
        o.checkpoint_tick = Some(Duration::from_millis(20));
        o.checkpoint_events = Some(1);
        let hub = Hub::open(o).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let written = loop {
            if let Some(m) = store::checkpoints(&conn).unwrap().into_iter().next() {
                break m;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no checkpoint written"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        hub.shutdown().await;
        drop(hub);
        let added: Vec<String> = store::load_with_gseq(&conn, None).unwrap()[before as usize..]
            .iter()
            .map(|(ev, _)| format!("{} {} {:?}", ev.thread, ev.kind(), ev.body.data().ok()))
            .collect();
        assert!(added.is_empty(), "the hub recorded {added:#?}");
        let full = fold_dump(Core::replay_full(&db, None).unwrap().0);
        let (got, replayed) = started(&db);
        assert_eq!(replayed.checkpoint, Some(written.id));
        assert_eq!(replayed.events, 0, "the log ends with a control event");
        assert!(got == full, "{}", diff(&got, &full));
    }

    /// `TWR_CHECKPOINT_CHECK=1`: as each hub of the crate's tests goes away, [`check_log`] on a
    /// copy of its log at up to 24 of its control events (the hub's `[notify]` settings): every scenario
    /// the tests make, folded from checkpoints. Off by default (it takes time).
    pub(in crate::hub) fn check_on_drop(db: &Path, notify: &NotifyConfig) {
        if std::env::var_os("TWR_CHECKPOINT_CHECK").is_none_or(|v| v.is_empty())
            || std::thread::panicking()
            || !db.exists()
        {
            return;
        }
        // On a copy: the test may still read the log, its checkpoints included.
        let Ok(copy) = tempfile::tempdir() else {
            return;
        };
        let db_copy = copy.path().join("twr.db");
        for ext in ["", "-wal"] {
            let from = format!("{}{ext}", db.display());
            if Path::new(&from).exists()
                && std::fs::copy(&from, format!("{}{ext}", db_copy.display())).is_err()
            {
                return;
            }
        }
        let db = db_copy.as_path();
        let ids = control_ids(db);
        if ids.is_empty() {
            return;
        }
        let step = ids.len().div_ceil(24);
        let mut positions: Vec<String> = ids.iter().step_by(step).cloned().collect();
        if positions.last() != ids.last() {
            positions.push(ids.last().unwrap().clone());
        }
        if let Err(e) = check_log(db, notify, &positions) {
            panic!("checkpoints of {}: {e}", db.display());
        }
        eprintln!(
            "checkpoints of a log of {} control events: the same state from {} of them",
            ids.len(),
            positions.len()
        );
    }

    /// [`check_log`] on a copy of a real log (`TWR_CHECKPOINT_DB`, copied into a new directory
    /// under the temporary directory first): at evenly spread control events and at the last
    /// deletion, restore, orchestrator turn, worker turn and request; with and without a hook.
    /// Run in release mode: `cargo test --release -p twr-hub --lib -- --ignored --nocapture
    /// checkpoints_of_a_real_log`.
    #[test]
    #[ignore = "needs a copy of a real log: TWR_CHECKPOINT_DB"]
    fn checkpoints_of_a_real_log() {
        let src = std::path::PathBuf::from(
            std::env::var_os("TWR_CHECKPOINT_DB").expect("TWR_CHECKPOINT_DB"),
        );
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        for ext in ["", "-wal"] {
            let from = format!("{}{ext}", src.display());
            if std::path::Path::new(&from).exists() {
                std::fs::copy(&from, format!("{}{ext}", db.display())).unwrap();
            }
        }
        // (Migrates the copy.)
        let store = Store::open(&db).unwrap();
        let log = store.load_all_with_gseq().unwrap();
        let orchestrators: HashSet<&str> = log
            .iter()
            .filter_map(|(ev, _)| match &ev.body {
                EventBody::ThreadCreated(p) if twr_ext_role(&p.ext) == Some("orchestrator") => {
                    Some(ev.thread.as_str())
                }
                _ => None,
            })
            .collect();
        let controls: Vec<&Event> = log
            .iter()
            .filter(|(_, g)| g.is_some())
            .map(|(ev, _)| ev)
            .collect();
        let last =
            |f: &dyn Fn(&Event) -> bool| controls.iter().rev().find(|e| f(e)).map(|e| e.id.clone());
        let mut picked: Vec<String> = [
            last(&|e| matches!(e.body, EventBody::ThreadDeleted(_))),
            last(&|e| matches!(e.body, EventBody::ThreadRestored(_))),
            last(&|e| {
                matches!(e.body, EventBody::TurnStarted(_))
                    && orchestrators.contains(e.thread.as_str())
            }),
            last(&|e| {
                matches!(e.body, EventBody::TurnStarted(_))
                    && !orchestrators.contains(e.thread.as_str())
            }),
            last(&|e| matches!(e.body, EventBody::AttentionRaised(_))),
        ]
        .into_iter()
        .flatten()
        .collect();
        let step = controls.len().div_ceil(10).max(1);
        picked.extend(controls.iter().step_by(step).map(|e| e.id.clone()));
        picked.push(controls.last().unwrap().id.clone());
        let order: HashMap<&str, usize> = controls
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.as_str(), i))
            .collect();
        picked.sort_by_key(|id| order[id.as_str()]);
        picked.dedup();
        eprintln!(
            "{} events, {} control; checking at {} of them",
            log.len(),
            controls.len(),
            picked.len()
        );
        drop((log, store));
        for notify in [NotifyConfig::default(), with_hook()] {
            let t = std::time::Instant::now();
            let states = check_log(&db, &notify, &picked).unwrap();
            let sizes: Vec<usize> = states.iter().map(|s| s.to_string().len()).collect();
            eprintln!(
                "hook {}: ok in {:.1} s; states {:?} bytes",
                if notify.command.is_empty() {
                    "off"
                } else {
                    "on"
                },
                t.elapsed().as_secs_f64(),
                sizes
            );
        }
    }

    /// `thread.created` `ext.twr.role`.
    fn twr_ext_role(ext: &Option<JsonMap>) -> Option<&str> {
        ext.as_ref()?.get("twr")?.get("role")?.as_str()
    }

    /// The fields of a checkpoint's state that are not empty in any of `states`.
    fn filled(states: &[Value]) -> HashSet<String> {
        let mut out = HashSet::new();
        let empty = |v: &Value| match v {
            Value::Null => true,
            Value::Object(m) => m.is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Bool(b) => !b,
            _ => false,
        };
        for s in states {
            for (k, v) in s.as_object().unwrap() {
                // One level down for the containers of several things.
                if let ("orch" | "disk" | "after" | "wake" | "hung" | "registry", Some(m)) =
                    (k.as_str(), v.as_object())
                {
                    for (k2, v2) in m {
                        if !empty(v2) {
                            out.insert(format!("{k}.{k2}"));
                        }
                    }
                } else if !empty(v) {
                    out.insert(k.clone());
                }
            }
        }
        out
    }

    /// From a checkpoint at any control event of a log that goes through what the hub folds
    /// (with and without the notification hook), a hub start folds the state a fold of the
    /// whole log gives, byte for byte; and the checkpoints there held every kind of state.
    #[test]
    fn a_start_from_any_checkpoint_folds_what_the_whole_log_does() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, false);
        let positions = control_ids(&db);
        assert!(positions.len() > 50, "{}", positions.len());
        let mut states = Vec::new();
        for notify in [NotifyConfig::default(), with_hook()] {
            states.extend(check_log(&db, &notify, &positions).unwrap());
        }
        // What a thread's view holds inside, in some checkpoint: its background tasks, what its
        // turns spent, a compaction under way.
        let threads = |s: &Value| -> Vec<Value> {
            s["registry"]["threads"]
                .as_object()
                .map(|m| m.values().cloned().collect())
                .unwrap_or_default()
        };
        let some = |f: &dyn Fn(&Value) -> bool| states.iter().flat_map(threads).any(|v| f(&v));
        assert!(some(&|v| v["background_tasks"]
            .as_object()
            .is_some_and(|m| !m.is_empty())));
        assert!(some(&|v| v["spent"]["cost"]
            .as_f64()
            .is_some_and(|c| c > 0.29)));
        assert!(some(&|v| !v["compacting"].is_null()));
        let filled = filled(&states);
        for field in [
            "registry.threads",
            "registry.deleted",
            "attention",
            "usage",
            "known_attn",
            "adapter_attn",
            "rt",
            "orch.thread",
            "orch.attn_gseq",
            "orch.closed_blocking",
            "orch.lives",
            "orch.user_modes",
            "orch.arrivals",
            "orch.latest_arrival",
            "orch.turn",
            "orch.notices",
            "orch.wake",
            "orch.watches",
            "orch.watch_count",
            "orch.idle_fired",
            "orch.taken_back",
            "orch.withdrawn_turns",
            "orch.withdrawn_at",
            "orch.maintenance_turns",
            "hub_questions",
            "hub_cards",
            "delete_cards",
            "disk.cards",
            "hook",
            "after.pending",
            "after.cards",
            "after.orphans",
            "card_outcomes",
            "context_hashes",
            "project_contexts",
            "hub_errors",
            "stalled",
            "wake.ended",
            "wake.woken",
            "wake.chain",
            "wake.compacting",
            "hung.cards",
            "hung.answered",
            "went_on",
            "activity",
            "trash",
            "deleted_rt",
            "items_deleted",
            "items_restored",
        ] {
            assert!(
                filled.contains(field),
                "no checkpoint had {field}: {filled:?}"
            );
        }
    }
}
