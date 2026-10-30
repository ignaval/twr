//! Archiving and pruning the log (`[log]`): what agents said, thought and did long ago leaves
//! `twr.db`, so that the hub's log stays the size of what it reads.
//!
//! **What moves.** Only agent events (`agent.text`, `agent.thinking`, `agent.tool_call`,
//! `agent.tool_result`, [`store::AGENT_KINDS`]); control events never do (a schema 9 trigger
//! refuses to delete any other row), and neither does anything the hub reads again:
//!
//! - of a deleted thread whose trash item of its latest deletion is purged (`trash.purged`,
//!   `hub::trash`): all of them. It is never restored, and nothing reads them. (Known
//!   limitation: a remote box's purge whose answer was lost is never recorded, so such a
//!   thread's events only go by the next rule.);
//! - of any other thread: those of each turn that ended more than `[log] retention_days` ago
//!   (at least [`twr_core::config::MIN_LOG_RETENTION_DAYS`]: a remote box's recorded commands
//!   are resent within [`twr_core::runner::RESEND_HORIZON`], and a resend asks whether the
//!   turn's runtime recorded anything) and whose reactions are complete: a worker's turn with
//!   its `report.ready` (or one that was interrupted, which gets none), an orchestrator's turn
//!   that did not fail or whose `error` item is raised (startup recovery reads the tool calls
//!   and final text of any other); never the thread's last turn, never an event recorded after
//!   the start of that last turn, never a final `agent.text` (handoffs, reactions, suggestions,
//!   glances and transcripts read those), and never a turn a recorded command for a remote box
//!   still names.
//!
//! What the hub folds from agent events ([`Core::apply`]) is the open turn's activity and
//! what a thread's latest events say: both come from the last turn, which stays, or, for a
//! purged thread, are dropped when the purge is folded. So a fold of the log after a
//! compaction is the one before it, and checkpoints stay valid (the tests compare both).
//!
//! **Blobs.** Mark and sweep: every blob pointer in a row that stays (its `data` and `raw`,
//! also the checkpoints, the queued prompts, the recorded commands and the spawn requests),
//! and every pointer in a blob reached that way, keeps its blob; every other blob of `twr.db`
//! moves (or goes). The archive holds every blob its rows reach, nested ones and those a row
//! that stays shares too (copied before the rows, so no archived row ever lacks them), so it
//! never needs `twr.db`, also once `[log] archive = false`.
//!
//! **Order.** The rows are copied into `archive.db` ([`crate::archive`], `INSERT OR IGNORE`)
//! and committed (synchronous FULL) first; then deleted from `twr.db`, each only once its copy
//! is in the archive, in short transactions (at most [`PER_TRANSACTION`] rows, committed once
//! [`WRITE_BUDGET`] went into one) with a pause between them. All of it runs on connections
//! of its own, in a blocking task, under no lock of the hub's: the hub keeps writing meanwhile
//! (its writes wait out a transaction's write lock: 85 ms at most on a copy of a real log, the
//! write-ahead log being copied into `twr.db` between transactions, never in a commit, and at
//! most 8 MB of blobs going in one). Nothing is scanned while that lock is held: the reads
//! (the plan, the mark) run in read transactions, which never stop the hub's writes. A crash
//! in between leaves a row in both databases (readers take `twr.db`'s); the next run deletes
//! it. Blobs go the same way, after the rows; a blob something recorded meanwhile points to is
//! kept (before each transaction the rows and blobs recorded since the last look, the queued
//! prompts, recorded commands and spawn requests are read; once its lock is held, the few rows
//! and blobs recorded since that read are, and whether a prompt, command or spawn came since,
//! by their newest ids: if one did, nothing is deleted then and they are read again outside
//! it). With `[log] archive = false` nothing is copied.
//!
//! **Space.** SQLite reuses the pages freed. `twr log compact --vacuum` also runs `VACUUM`
//! (it rewrites `twr.db` whole, holding the hub's lock: the hub waits for it), which turns on
//! `auto_vacuum = INCREMENTAL` the first time; from then on each run gives back at most
//! [`INCREMENTAL_PAGES`] free pages ([`INCREMENTAL_STEP`] per transaction). `VACUUM` keeps the
//! order of the rows (the replay reads them in rowid order, and checkpoints find their last
//! event by id).
//!
//! **When.** [`PRUNE_TICK`] (hourly, the first time [`FIRST_LOOK`] after the hub opens), when
//! [`BATCH`] rows or more can move; and by hand, `twr log compact` (`log.compact`, users
//! only). `twr log stats` (`log.stats`) says how big the log is and what the next run moves.
//! A compaction and a checkpoint's replay never run at once ([`Inner::maintenance`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::Serialize;
use serde_json::{json, Value};
use twr_core::event::{Event, HUB_THREAD};
use twr_core::proto::{codes, RpcError};

use super::{Caller, Core, Hub, HubState, Inner};
use crate::archive;
use crate::store::{self, AGENT_KINDS};

/// How often the hub looks whether its log has something to move.
pub const PRUNE_TICK: Duration = Duration::from_secs(3600);

/// How long after the hub opened it looks the first time (the tick when that is shorter).
pub const FIRST_LOOK: Duration = Duration::from_secs(600);

/// The fewest rows an automatic run moves.
pub const BATCH: usize = 2_000;

/// Rows (or blobs) one transaction deletes at most.
const PER_TRANSACTION: usize = 500;

/// How long one transaction on `twr.db` holds its write lock, about: it commits once that
/// much time went into it (the hub's writes wait for it meanwhile).
pub const WRITE_BUDGET: Duration = Duration::from_millis(30);

/// Bytes of blobs one transaction deletes at most (one larger blob goes alone).
const BYTES_PER_TRANSACTION: u64 = 8 << 20;

/// Rows (or blobs, a tenth of it) one copy into the archive takes: a write of `archive.db`,
/// which holds no lock of `twr.db`'s.
const COPY_CHUNK: usize = 2_000;

/// The pause between two transactions.
const PAUSE: Duration = Duration::from_millis(20);

/// Free pages one `PRAGMA incremental_vacuum` gives back.
pub const INCREMENTAL_STEP: u32 = 200;

/// Free pages a run gives back at most (incremental vacuum).
pub const INCREMENTAL_PAGES: u32 = 64_000;

/// The SQL list of [`AGENT_KINDS`].
fn agent_kinds_sql() -> String {
    AGENT_KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a run does.
#[derive(Debug, Clone)]
pub(super) struct Settings {
    /// `[log] retention_days`.
    pub(super) retention: Duration,
    /// `[log] archive`.
    pub(super) archive: bool,
    /// Now (the turns' ends are compared with it).
    pub(super) now: DateTime<Utc>,
    /// The fewest rows it moves (fewer: it moves nothing).
    pub(super) min_rows: usize,
    /// Run `VACUUM` after.
    pub(super) vacuum: bool,
    /// Drop the checkpoints first.
    pub(super) drop_checkpoints: bool,
    /// Rows (or blobs) one transaction deletes at most.
    pub(super) batch: usize,
    /// How long one write transaction runs, about ([`WRITE_BUDGET`]).
    pub(super) budget: Duration,
    /// The pause between transactions.
    pub(super) pause: Duration,
    /// Called on the run's connection once the blobs are marked, before any is moved (tests:
    /// what the hub writes meanwhile).
    pub(super) on_mark: Option<fn(&Connection)>,
    /// Called before each blob deletion takes the write lock, after the reads outside it
    /// (tests: what the hub writes in between).
    pub(super) before_lock: Option<fn(&Connection)>,
    /// Called once the archive has the blobs, before it gets the rows; an error ends the run
    /// there (tests: a crash in between).
    pub(super) between_copies: Option<fn(&Connection) -> anyhow::Result<()>>,
}

impl Settings {
    /// What `[log]` says, now: an automatic run's.
    pub(super) fn of(log: &twr_core::config::LogConfig) -> Settings {
        Settings {
            retention: log.retention(),
            archive: log.archive,
            now: Utc::now(),
            min_rows: BATCH,
            vacuum: false,
            drop_checkpoints: false,
            batch: PER_TRANSACTION,
            budget: WRITE_BUDGET,
            pause: PAUSE,
            on_mark: None,
            before_lock: None,
            between_copies: None,
        }
    }
}

/// One row a run moves.
#[derive(Debug, Clone)]
struct Pick {
    rowid: i64,
    id: String,
    kind: String,
    bytes: u64,
}

/// What a run would move ([`plan`]).
#[derive(Debug, Default)]
struct Plan {
    picks: Vec<Pick>,
    /// Purged deleted threads with something to move.
    purged_threads: usize,
    /// Other threads' turns with something to move.
    turns: usize,
}

impl Plan {
    fn bytes(&self) -> u64 {
        self.picks.iter().map(|p| p.bytes).sum()
    }

    /// `{kind: {rows, bytes}}`.
    fn by_kind(&self) -> BTreeMap<String, (u64, u64)> {
        let mut out: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for p in &self.picks {
            let e = out.entry(p.kind.clone()).or_default();
            e.0 += 1;
            e.1 += p.bytes;
        }
        out
    }
}

/// One turn of a thread, as [`plan`] reads it.
#[derive(Debug, Default)]
struct TurnFacts {
    /// Its latest `turn.ended`: when, and its result.
    ended: Option<(DateTime<Utc>, String)>,
    /// It has a `report.ready`.
    reported: bool,
    /// It has an `error` item the hub raised.
    error_item: bool,
}

/// One thread, as [`plan`] reads it.
#[derive(Debug, Default)]
struct ThreadFacts {
    orchestrator: bool,
    /// Deleted (and not restored since), with the trash item of that deletion.
    deleted: Option<Option<String>>,
    /// Its last turn.
    last_turn: u64,
    /// The row of its last `turn.started`.
    last_start: i64,
    turns: HashMap<u64, TurnFacts>,
}

/// The kinds [`plan`] reads facts from.
const FACT_KINDS: &str = "'thread.created', 'thread.deleted', 'thread.restored', 'trash.purged', \
                          'turn.started', 'turn.ended', 'report.ready', 'attention.raised'";

/// What a run with `s` would move from the log on `conn` (see the module docs).
fn plan(conn: &Connection, s: &Settings) -> anyhow::Result<Plan> {
    let mut threads: HashMap<String, ThreadFacts> = HashMap::new();
    // (item, thread) of each purge, as the fold matches them.
    let mut purged_items: HashSet<(String, String)> = HashSet::new();
    {
        let mut st = conn.prepare(&format!(
            "SELECT rowid, thread, kind, turn, ts, source,
                CASE kind WHEN 'thread.created' THEN json_extract(data, '$.ext.twr.role')
                          WHEN 'thread.deleted' THEN json_extract(data, '$.trash')
                          WHEN 'trash.purged' THEN json_extract(data, '$.item')
                          WHEN 'turn.ended' THEN json_extract(data, '$.result')
                          WHEN 'attention.raised' THEN json_extract(data, '$.reason') END,
                CASE kind WHEN 'trash.purged' THEN json_extract(data, '$.thread') END
             FROM events WHERE kind IN ({FACT_KINDS}) ORDER BY rowid"
        ))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let rowid: i64 = r.get(0)?;
            let thread: String = r.get(1)?;
            let kind: String = r.get(2)?;
            let turn: Option<u64> = r.get::<_, Option<i64>>(3)?.map(|t| t.max(0) as u64);
            let source: String = r.get(5)?;
            let what: Option<String> = r.get(6)?;
            if kind == "trash.purged" {
                let of: Option<String> = r.get(7)?;
                if let (true, Some(item), Some(of)) =
                    (thread == HUB_THREAD && source == "hub", what, of)
                {
                    purged_items.insert((item, of));
                }
                continue;
            }
            let t = threads.entry(thread).or_default();
            match (kind.as_str(), turn) {
                ("thread.created", _) => t.orchestrator = what.as_deref() == Some("orchestrator"),
                ("thread.deleted", _) => t.deleted = Some(what),
                ("thread.restored", _) => t.deleted = None,
                ("turn.started", Some(n)) => {
                    t.last_turn = t.last_turn.max(n);
                    t.last_start = rowid;
                }
                ("turn.ended", Some(n)) => {
                    let ts: String = r.get(4)?;
                    let ts = DateTime::parse_from_rfc3339(&ts)?.with_timezone(&Utc);
                    t.turns.entry(n).or_default().ended = Some((ts, what.unwrap_or_default()));
                }
                ("report.ready", Some(n)) => t.turns.entry(n).or_default().reported = true,
                ("attention.raised", Some(n))
                    if source == "hub" && what.as_deref() == Some("error") =>
                {
                    t.turns.entry(n).or_default().error_item = true
                }
                _ => {}
            }
        }
    }
    let outbox: HashSet<(String, u64)> = {
        let mut st =
            conn.prepare("SELECT thread, turn FROM remote_outbox WHERE turn IS NOT NULL")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let cutoff =
        s.now - chrono::Duration::from_std(s.retention).unwrap_or(chrono::Duration::MAX / 2);
    let purged: HashSet<&str> = threads
        .iter()
        .filter(|(id, t)| {
            matches!(&t.deleted, Some(Some(item)) if purged_items.contains(&(item.clone(), (*id).clone())))
        })
        .map(|(id, _)| id.as_str())
        .collect();
    // The turns whose agent events may move, by thread.
    let mut due: HashMap<&str, HashSet<u64>> = HashMap::new();
    for (id, t) in &threads {
        if purged.contains(id.as_str()) {
            continue;
        }
        for (n, f) in &t.turns {
            let Some((at, result)) = &f.ended else {
                continue;
            };
            let reacted = if t.orchestrator {
                result != "error" || f.error_item
            } else {
                f.reported || result == "interrupted"
            };
            if *n < t.last_turn && *at < cutoff && reacted && !outbox.contains(&(id.clone(), *n)) {
                due.entry(id.as_str()).or_default().insert(*n);
            }
        }
    }
    let mut plan = Plan::default();
    let mut moved_threads: HashSet<String> = HashSet::new();
    let mut moved_turns: HashSet<(String, u64)> = HashSet::new();
    let mut st = conn.prepare(&format!(
        "SELECT rowid, id, thread, turn, kind,
                length(CAST(data AS BLOB)) + COALESCE(length(raw), 0),
                CASE WHEN kind = 'agent.text' THEN json_extract(data, '$.final') END
         FROM events WHERE kind IN ({}) ORDER BY rowid",
        agent_kinds_sql()
    ))?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let rowid: i64 = r.get(0)?;
        let thread: String = r.get(2)?;
        let turn: Option<u64> = r.get::<_, Option<i64>>(3)?.map(|t| t.max(0) as u64);
        let is_final = r.get::<_, Option<i64>>(6)? == Some(1);
        let pick = if purged.contains(thread.as_str()) {
            moved_threads.insert(thread.clone());
            true
        } else {
            match (turn, threads.get(&thread), due.get(thread.as_str())) {
                (Some(n), Some(t), Some(turns)) if turns.contains(&n) => {
                    let ok = rowid < t.last_start && !is_final;
                    if ok {
                        moved_turns.insert((thread.clone(), n));
                    }
                    ok
                }
                _ => false,
            }
        };
        if pick {
            plan.picks.push(Pick {
                rowid,
                id: r.get(1)?,
                kind: r.get(4)?,
                bytes: r.get::<_, i64>(5)?.max(0) as u64,
            });
        }
    }
    plan.purged_threads = moved_threads.len();
    plan.turns = moved_turns.len();
    Ok(plan)
}

/// What a run did.
#[derive(Debug, Default, Serialize)]
pub(super) struct Report {
    /// Agent events moved out of `twr.db`.
    pub(super) rows: usize,
    /// Their size (`data` and `raw`), in bytes.
    pub(super) bytes: u64,
    /// `{kind: {rows, bytes}}`.
    pub(super) kinds: BTreeMap<String, Value>,
    /// Purged deleted threads they were of.
    pub(super) purged_threads: usize,
    /// Other threads' turns they were of.
    pub(super) turns: usize,
    /// Blobs moved out (no row that stays reaches them).
    pub(super) blobs: usize,
    /// Their size, in bytes.
    pub(super) blob_bytes: u64,
    /// Copied into `archive.db` (`false`: deleted).
    pub(super) archived: bool,
    /// Transactions on `twr.db` (rows and blobs).
    pub(super) batches: usize,
    /// The longest of them, in milliseconds: how long it held the write lock at most.
    pub(super) longest_batch_ms: u64,
    /// Which that was: `rows`, `blobs` or `incremental_vacuum`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) longest_batch: Option<&'static str>,
    /// Planning, copying, deleting, sweeping blobs and the whole run, in milliseconds.
    pub(super) plan_ms: u64,
    pub(super) copy_ms: u64,
    pub(super) delete_ms: u64,
    pub(super) sweep_ms: u64,
    pub(super) total_ms: u64,
    /// Free pages given back (`PRAGMA incremental_vacuum`).
    pub(super) freed_pages: u64,
    /// The `VACUUM`, when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) vacuum: Option<VacuumReport>,
    /// Checkpoints dropped (`--drop-checkpoints`).
    pub(super) checkpoints_dropped: usize,
    /// Why it moved nothing, when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) skipped: Option<String>,
}

/// What a `VACUUM` did.
#[derive(Debug, Default, Serialize)]
pub(super) struct VacuumReport {
    /// How long it took (the hub waited that long), in milliseconds.
    pub(super) ms: u64,
    /// `twr.db`'s size before and after, in bytes.
    pub(super) before: u64,
    pub(super) after: u64,
    /// It turned `auto_vacuum = INCREMENTAL` on.
    pub(super) incremental_set: bool,
}

/// Copies what the write-ahead logs hold into `twr.db` and `archive.db` (`PASSIVE`: it takes
/// no lock that stops the hub's writes, and copies what no reader still needs), outside any
/// transaction.
fn checkpoint(conn: &Connection) -> anyhow::Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
    Ok(())
}

impl Report {
    /// Counts a write transaction on `twr.db` (of `what`) that held its lock for `d`.
    fn held(&mut self, what: &'static str, d: Duration) {
        self.batches += 1;
        if self.longest_batch.is_none() || ms(d) > self.longest_batch_ms {
            self.longest_batch_ms = ms(d);
            self.longest_batch = Some(what);
        }
    }
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The size of `path` in bytes (0: none).
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// `path` with `suffix` added (`twr.db-wal`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// Runs a compaction of the log at `db` (see the module docs) with `s`. `hold` is taken for
/// the `VACUUM` only: the hub's lock, so that the hub waits for it rather than failing on its
/// busy timeout (`None`: a log no hub has open).
pub(super) fn compact(
    db: &Path,
    s: &Settings,
    hold: Option<&std::sync::Mutex<Core>>,
) -> anyhow::Result<Report> {
    let started = Instant::now();
    let mut report = Report {
        archived: s.archive,
        ..Report::default()
    };
    let mut conn = store::connect(db)?;
    // The write-ahead log is copied into `twr.db` between transactions ([`checkpoint`]), not
    // in a commit, so no commit holds the write lock while it copies.
    conn.pragma_update(None, "wal_autocheckpoint", 0)?;
    if s.drop_checkpoints {
        report.checkpoints_dropped = store::drop_checkpoints(&conn)?;
    }
    let t = Instant::now();
    let plan = plan(&conn, s)?;
    report.plan_ms = ms(t.elapsed());
    if s.archive {
        drop(archive::open(&archive::path_of(db))?);
        conn.execute(
            "ATTACH DATABASE ?1 AS archive",
            [archive::path_of(db).to_string_lossy()],
        )?;
        conn.execute_batch("PRAGMA archive.synchronous = FULL;")?;
    }
    let enough = plan.picks.len() >= s.min_rows.max(1);
    if enough {
        move_rows(&mut conn, &plan, s, &mut report)?;
    } else {
        report.skipped = Some(match plan.picks.len() {
            0 => "no agent events to move".to_string(),
            n => format!("{n} agent events to move, fewer than {}", s.min_rows),
        });
    }
    // The blobs, after moved rows, and on every run by hand (blobs a run cut short left).
    if enough || s.min_rows <= 1 {
        let t = Instant::now();
        sweep(&mut conn, s, &mut report)?;
        report.sweep_ms = ms(t.elapsed());
    }
    if s.vacuum {
        report.vacuum = Some(vacuum(&conn, db, hold)?);
    } else {
        report.freed_pages = incremental_vacuum(&conn, s.pause, &mut report)?;
    }
    report.total_ms = ms(started.elapsed());
    Ok(report)
}

/// Copies the planned rows into the archive (when `s.archive`), then deletes them from
/// `twr.db` in transactions of at most [`Settings::batch`] rows and about
/// [`Settings::budget`].
fn move_rows(
    conn: &mut Connection,
    plan: &Plan,
    s: &Settings,
    report: &mut Report,
) -> anyhow::Result<()> {
    let batch = s.batch.max(1);
    let t = Instant::now();
    if s.archive {
        // First the blobs they reach, nested ones too, even those a row that stays shares:
        // the archive never needs `twr.db` (whose copy goes once no row of its own reaches
        // it, `[log] archive = false` from then on included). Committed before the rows, so
        // no archived row ever points to a blob the archive lacks.
        let mut refs = HashSet::new();
        for chunk in plan.picks.chunks(COPY_CHUNK) {
            let rowids: Vec<i64> = chunk.iter().map(|p| p.rowid).collect();
            let mut st = conn.prepare_cached(
                "SELECT data, raw FROM main.events WHERE rowid IN (SELECT value FROM json_each(?1))",
            )?;
            let mut rows = st.query([serde_json::to_string(&rowids)?])?;
            while let Some(r) = rows.next()? {
                refs_in(r.get_ref(0)?.as_bytes()?, &mut refs);
                if let Some(raw) = r.get_ref(1)?.as_bytes_or_null()? {
                    refs_in(raw, &mut refs);
                }
            }
        }
        let refs: Vec<String> = blob_closure(conn, refs)?.into_iter().collect();
        for chunk in refs.chunks(COPY_CHUNK / 10) {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO archive.blobs SELECT hash, bytes FROM main.blobs
                 WHERE hash IN (SELECT value FROM json_each(?1))",
                [serde_json::to_string(chunk)?],
            )?;
            tx.commit()?;
            checkpoint(conn)?;
        }
        if let Some(f) = s.between_copies {
            f(conn)?;
        }
        for chunk in plan.picks.chunks(COPY_CHUNK) {
            let rowids: Vec<i64> = chunk.iter().map(|p| p.rowid).collect();
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO archive.events
                 SELECT thread, seq, id, v, ts, turn, source, kind, data, raw FROM main.events
                 WHERE rowid IN (SELECT value FROM json_each(?1))",
                [serde_json::to_string(&rowids)?],
            )?;
            tx.commit()?;
            checkpoint(conn)?;
        }
    }
    report.copy_ms = ms(t.elapsed());
    let t = Instant::now();
    // Each row goes only once its copy is in the archive: a row whose copy is not there (the
    // archive was replaced meanwhile) stays for the next run.
    let delete = if s.archive {
        "DELETE FROM main.events WHERE rowid = ?1 AND id = ?2
         AND EXISTS (SELECT 1 FROM archive.events a WHERE a.id = ?2)"
    } else {
        "DELETE FROM main.events WHERE rowid = ?1 AND id = ?2"
    };
    let mut next = 0;
    while next < plan.picks.len() {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // (Held from here: what the hub writes waits.)
        let t = Instant::now();
        {
            let mut st = tx.prepare_cached(delete)?;
            let first = next;
            while next < plan.picks.len() {
                let p = &plan.picks[next];
                next += 1;
                if st.execute(params![p.rowid, p.id])? == 1 {
                    report.rows += 1;
                    report.bytes += p.bytes;
                    let e = report
                        .kinds
                        .entry(p.kind.clone())
                        .or_insert_with(|| json!({"rows": 0, "bytes": 0}));
                    e["rows"] = json!(e["rows"].as_u64().unwrap_or(0) + 1);
                    e["bytes"] = json!(e["bytes"].as_u64().unwrap_or(0) + p.bytes);
                }
                if next - first >= batch || t.elapsed() >= s.budget {
                    break;
                }
            }
        }
        tx.commit()?;
        report.held("rows", t.elapsed());
        checkpoint(conn)?;
        std::thread::sleep(s.pause);
    }
    report.purged_threads = plan.purged_threads;
    report.turns = plan.turns;
    report.delete_ms = ms(t.elapsed());
    Ok(())
}

/// Adds to `out` every blob pointer in `bytes` (`sha256:` and 64 lowercase hex digits).
fn refs_in(bytes: &[u8], out: &mut HashSet<String>) {
    const PREFIX: &[u8] = twr_core::blob::REF_PREFIX.as_bytes();
    let len = PREFIX.len() + 64;
    let mut i = 0;
    while i + len <= bytes.len() {
        match bytes[i..].iter().position(|b| *b == PREFIX[0]) {
            None => return,
            Some(at) => i += at,
        }
        if i + len > bytes.len() {
            return;
        }
        if bytes[i..].starts_with(PREFIX) {
            if let Ok(r) = std::str::from_utf8(&bytes[i..i + len]) {
                if twr_core::blob::is_blob_ref(r) {
                    out.insert(r.to_string());
                    i += len;
                    continue;
                }
            }
        }
        i += 1;
    }
}

/// The pointers held by every row of `twr.db` other than events, and by the events with
/// rowid above `after` (all for `None`) up to `upto`.
fn roots(
    conn: &Connection,
    after: i64,
    upto: Option<i64>,
    out: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let mut st = conn.prepare_cached(
        "SELECT data, raw FROM events WHERE rowid > ?1 AND rowid <= ?2
         AND (raw IS NOT NULL OR instr(data, 'sha256:') > 0)",
    )?;
    let mut rows = st.query(params![after, upto.unwrap_or(i64::MAX)])?;
    while let Some(r) = rows.next()? {
        refs_in(r.get_ref(0)?.as_bytes()?, out);
        if let Some(raw) = r.get_ref(1)?.as_bytes_or_null()? {
            refs_in(raw, out);
        }
    }
    Ok(())
}

/// The pointers held by everything else of `twr.db`: checkpoints, queued prompts, recorded
/// commands and spawn operations.
fn other_roots(conn: &Connection, out: &mut HashSet<String>) -> anyhow::Result<()> {
    let mut st =
        conn.prepare_cached("SELECT state FROM checkpoints WHERE instr(state, 'sha256:') > 0")?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        refs_in(r.get_ref(0)?.as_bytes()?, out);
    }
    live_roots(conn, out)
}

/// The pointers held by what the hub writes besides events while a run goes on (checkpoints
/// are not: none is written meanwhile, [`Inner::maintenance`]): queued prompts, recorded
/// commands, spawn operations. Small tables, read whole.
fn live_roots(conn: &Connection, out: &mut HashSet<String>) -> anyhow::Result<()> {
    for sql in [
        "SELECT prompt FROM pending_prompts WHERE instr(prompt, 'sha256:') > 0",
        "SELECT op FROM remote_outbox WHERE instr(op, 'sha256:') > 0",
        // (A spawn's request; its result is the hub's own answer, a thread object or an error,
        // never a payload pointer.)
        "SELECT request FROM spawn_operations WHERE instr(request, 'sha256:') > 0",
    ] {
        let mut st = conn.prepare_cached(sql)?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            refs_in(r.get_ref(0)?.as_bytes()?, out);
        }
    }
    Ok(())
}

/// What tells whether [`live_roots`] may have changed: the newest queued prompt, recorded
/// command and spawn operation (rows only ever added with higher ids, the columns read never
/// changed). Looked up through their keys: cheap whatever their size.
fn live_marker(conn: &Connection) -> anyhow::Result<(i64, i64, i64)> {
    Ok(conn.query_row(
        "SELECT (SELECT COALESCE(MAX(id), 0) FROM pending_prompts),
                (SELECT COALESCE(MAX(id), 0) FROM remote_outbox),
                (SELECT COALESCE(MAX(rowid), 0) FROM spawn_operations)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

/// The pointers inside each blob with rowid in `(after, upto]` that holds any: `{blob: [ref]}`.
fn nested(
    conn: &Connection,
    after: i64,
    upto: i64,
) -> anyhow::Result<HashMap<String, HashSet<String>>> {
    let mut st = conn.prepare_cached(
        "SELECT hash, bytes FROM blobs WHERE rowid > ?1 AND rowid <= ?2
         AND instr(bytes, 'sha256:') > 0",
    )?;
    let mut rows = st.query([after, upto])?;
    let mut out = HashMap::new();
    while let Some(r) = rows.next()? {
        let mut refs = HashSet::new();
        refs_in(r.get_ref(1)?.as_bytes()?, &mut refs);
        if !refs.is_empty() {
            out.insert(r.get(0)?, refs);
        }
    }
    Ok(out)
}

/// `refs` and every pointer inside the blobs of `twr.db` they reach, recursively.
fn blob_closure(conn: &Connection, refs: HashSet<String>) -> anyhow::Result<HashSet<String>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut todo: Vec<String> = refs.into_iter().collect();
    while !todo.is_empty() {
        let batch: Vec<String> = todo
            .drain(..todo.len().min(500))
            .filter(|r| seen.insert(r.clone()))
            .collect();
        if batch.is_empty() {
            continue;
        }
        let mut st = conn.prepare_cached(
            "SELECT bytes FROM main.blobs WHERE hash IN (SELECT value FROM json_each(?1))
             AND instr(bytes, 'sha256:') > 0",
        )?;
        let mut rows = st.query([serde_json::to_string(&batch)?])?;
        let mut found = HashSet::new();
        while let Some(r) = rows.next()? {
            refs_in(r.get_ref(0)?.as_bytes()?, &mut found);
        }
        todo.extend(found.into_iter().filter(|r| !seen.contains(r)));
    }
    Ok(seen)
}

/// `roots` and every pointer reached from them through `edges`.
fn closure(roots: HashSet<String>, edges: &HashMap<String, HashSet<String>>) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut todo: Vec<String> = roots.into_iter().collect();
    while let Some(r) = todo.pop() {
        if let Some(next) = edges.get(&r) {
            todo.extend(next.iter().filter(|n| !seen.contains(*n)).cloned());
        }
        seen.insert(r);
    }
    seen
}

/// Moves (or deletes) every blob of `twr.db` that no row that stays reaches (see the module
/// docs).
fn sweep(conn: &mut Connection, s: &Settings, report: &mut Report) -> anyhow::Result<()> {
    // The mark, in one read transaction.
    let (rows_upto, blobs_upto, edges, mut reachable, unreachable) = {
        let tx = conn.transaction()?;
        let rows_upto: i64 =
            tx.query_row("SELECT COALESCE(MAX(rowid), 0) FROM events", [], |r| {
                r.get(0)
            })?;
        let blobs_upto: i64 =
            tx.query_row("SELECT COALESCE(MAX(rowid), 0) FROM blobs", [], |r| {
                r.get(0)
            })?;
        let mut found = HashSet::new();
        roots(&tx, i64::MIN, Some(rows_upto), &mut found)?;
        other_roots(&tx, &mut found)?;
        let edges = nested(&tx, i64::MIN, blobs_upto)?;
        let reachable = closure(found, &edges);
        let mut unreachable: Vec<(String, u64)> = Vec::new();
        {
            let mut st = tx.prepare("SELECT hash, length(bytes) FROM blobs WHERE rowid <= ?1")?;
            let mut rows = st.query([blobs_upto])?;
            while let Some(r) = rows.next()? {
                let hash: String = r.get(0)?;
                if !reachable.contains(&hash) {
                    unreachable.push((hash, r.get::<_, i64>(1)?.max(0) as u64));
                }
            }
        }
        tx.commit()?;
        (rows_upto, blobs_upto, edges, reachable, unreachable)
    };
    if let Some(f) = s.on_mark {
        f(conn);
    }
    if unreachable.is_empty() {
        return Ok(());
    }
    if s.archive {
        for chunk in unreachable.chunks(COPY_CHUNK / 10) {
            let hashes: Vec<&str> = chunk.iter().map(|(h, _)| h.as_str()).collect();
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO archive.blobs SELECT hash, bytes FROM main.blobs
                 WHERE hash IN (SELECT value FROM json_each(?1))",
                [serde_json::to_string(&hashes)?],
            )?;
            tx.commit()?;
            checkpoint(conn)?;
        }
    }
    let delete = if s.archive {
        "DELETE FROM main.blobs WHERE hash = ?1
         AND EXISTS (SELECT 1 FROM archive.blobs a WHERE a.hash = ?1)"
    } else {
        "DELETE FROM main.blobs WHERE hash = ?1"
    };
    // What the hub recorded since the mark keeps what it points to: read outside any write
    // transaction up to the newest row, then, once the write lock is held (nothing more comes
    // before the deletes commit), only what came after that, a few rows at most.
    let mut seen = (rows_upto, blobs_upto);
    let mut next = 0;
    while next < unreachable.len() {
        // Outside any write transaction: the queued prompts, recorded commands and spawn
        // operations (read whole), and what was recorded since the last look.
        let marker = live_marker(conn)?;
        let mut fresh = HashSet::new();
        live_roots(conn, &mut fresh)?;
        if !fresh.is_empty() {
            reachable.extend(closure(fresh, &edges));
        }
        catch_up(conn, &mut seen, &edges, &mut reachable)?;
        if let Some(f) = s.before_lock {
            f(conn);
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let t = Instant::now();
        // Under the lock, only what is cheap: what came since that read (a few rows), and
        // whether a prompt, command or spawn came since; then nothing is deleted this time,
        // and they are read again outside it.
        if live_marker(&tx)? != marker {
            drop(tx);
            continue;
        }
        catch_up(&tx, &mut seen, &edges, &mut reachable)?;
        {
            let mut st = tx.prepare_cached(delete)?;
            let mut count = 0;
            let mut freed = 0u64;
            while next < unreachable.len() {
                let (hash, bytes) = &unreachable[next];
                // At most the budget's bytes, and a larger blob alone.
                if count > 0 && freed + bytes > BYTES_PER_TRANSACTION {
                    break;
                }
                next += 1;
                if !reachable.contains(hash) && st.execute([hash])? == 1 {
                    report.blobs += 1;
                    report.blob_bytes += bytes;
                    freed += bytes;
                    count += 1;
                }
                if count >= s.batch.max(1) || t.elapsed() >= s.budget {
                    break;
                }
            }
        }
        tx.commit()?;
        report.held("blobs", t.elapsed());
        checkpoint(conn)?;
        std::thread::sleep(s.pause);
    }
    Ok(())
}

/// Adds to `reachable` what the rows and blobs recorded after `seen` (the newest event and
/// blob rows looked at) point to, through `edges` and their own pointers; `seen` moves to the
/// newest ones now.
fn catch_up(
    conn: &Connection,
    seen: &mut (i64, i64),
    edges: &HashMap<String, HashSet<String>>,
    reachable: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let rows: i64 = conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM events", [], |r| {
        r.get(0)
    })?;
    let blobs: i64 = conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM blobs", [], |r| {
        r.get(0)
    })?;
    let mut fresh = HashSet::new();
    if rows > seen.0 {
        roots(conn, seen.0, Some(rows), &mut fresh)?;
    }
    // A new blob's own pointers count as reached: only a new row can point to it.
    if blobs > seen.1 {
        for refs in nested(conn, seen.1, blobs)?.into_values() {
            fresh.extend(refs);
        }
    }
    *seen = (seen.0.max(rows), seen.1.max(blobs));
    if !fresh.is_empty() {
        reachable.extend(closure(fresh, edges));
    }
    Ok(())
}

/// Gives back free pages of `twr.db` when it is in `auto_vacuum = INCREMENTAL`: at most
/// [`INCREMENTAL_PAGES`], [`INCREMENTAL_STEP`] per transaction. Returns how many.
fn incremental_vacuum(
    conn: &Connection,
    pause: Duration,
    report: &mut Report,
) -> anyhow::Result<u64> {
    let mode: i64 = conn.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    if mode != 2 {
        return Ok(0);
    }
    let mut freed = 0u64;
    while freed < u64::from(INCREMENTAL_PAGES) {
        let free: i64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
        if free <= 0 {
            break;
        }
        let t = Instant::now();
        conn.execute_batch(&format!(
            "PRAGMA main.incremental_vacuum({INCREMENTAL_STEP});"
        ))?;
        report.held("incremental_vacuum", t.elapsed());
        checkpoint(conn)?;
        let after: i64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
        if after >= free {
            break;
        }
        freed += (free - after) as u64;
        std::thread::sleep(pause);
    }
    Ok(freed)
}

/// `VACUUM` of `twr.db` (with `auto_vacuum = INCREMENTAL` set first, the first time), holding
/// `hold`, then a checkpoint that empties its write-ahead log.
fn vacuum(
    conn: &Connection,
    db: &Path,
    hold: Option<&std::sync::Mutex<Core>>,
) -> anyhow::Result<VacuumReport> {
    let size = || file_size(db) + file_size(&with_suffix(db, "-wal"));
    let before = size();
    let mode: i64 = conn.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    let incremental_set = mode != 2;
    let t = Instant::now();
    {
        let _held = hold.map(|m| m.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        if incremental_set {
            conn.execute_batch("PRAGMA main.auto_vacuum = INCREMENTAL;")?;
        }
        conn.execute_batch("VACUUM main;")?;
        conn.query_row("PRAGMA main.wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    }
    Ok(VacuumReport {
        ms: ms(t.elapsed()),
        before,
        after: size(),
        incremental_set,
    })
}

/// The sizes of the log at `db` and what a run with `s` would move (`log.stats`, without its
/// control backlog part).
pub(super) fn stats(db: &Path, s: &Settings) -> anyhow::Result<Value> {
    let conn = store::connect(db)?;
    let files =
        |p: &Path| json!({ "bytes": file_size(p), "wal": file_size(&with_suffix(p, "-wal")) });
    let kinds = |conn: &Connection| -> anyhow::Result<Value> {
        let mut st = conn.prepare(
            "SELECT kind, COUNT(*), SUM(length(CAST(data AS BLOB)) + COALESCE(length(raw), 0))
             FROM events GROUP BY kind ORDER BY 3 DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })?;
        let mut out = serde_json::Map::new();
        for r in rows {
            let (k, n, b) = r?;
            out.insert(k, json!({ "rows": n, "bytes": b }));
        }
        Ok(Value::Object(out))
    };
    let blobs = |conn: &Connection| -> anyhow::Result<Value> {
        let (n, b): (i64, Option<i64>) =
            conn.query_row("SELECT COUNT(*), SUM(length(bytes)) FROM blobs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        Ok(json!({ "count": n, "bytes": b.unwrap_or(0) }))
    };
    let pragma = |name: &str| -> anyhow::Result<i64> {
        Ok(conn.pragma_query_value(None, name, |r| r.get(0))?)
    };
    let auto_vacuum = match pragma("auto_vacuum")? {
        0 => "none",
        1 => "full",
        _ => "incremental",
    };
    let now = Utc::now();
    let head = store::control_head(&conn)?;
    let checkpoints: Vec<Value> = store::checkpoints(&conn)?
        .into_iter()
        .map(|c| {
            let at = DateTime::parse_from_rfc3339(&c.created)
                .map(|t| t.with_timezone(&Utc))
                .ok();
            let after = store::event_position(&conn, &c.last_event)
                .ok()
                .flatten()
                .and_then(|(row, _)| store::events_after(&conn, Some(row)).ok());
            json!({
                "id": c.id,
                "created": c.created,
                "age_secs": at.map(|t| (now - t).num_seconds().max(0)),
                "bytes": c.bytes,
                "gseq": c.gseq,
                "behind_gseq": head.saturating_sub(c.gseq),
                "events_after": after,
                "build": c.build,
            })
        })
        .collect();
    let plan = plan(&conn, s)?;
    let next: BTreeMap<String, Value> = plan
        .by_kind()
        .into_iter()
        .map(|(k, (n, b))| (k, json!({ "rows": n, "bytes": b })))
        .collect();
    let archive_path = archive::path_of(db);
    let archived = match archive::open_reader(&archive_path) {
        Some(a) => json!({
            "path": archive_path,
            "bytes": file_size(&archive_path),
            "wal": file_size(&with_suffix(&archive_path, "-wal")),
            "kinds": kinds(&a)?,
            "blobs": blobs(&a)?,
        }),
        None => Value::Null,
    };
    let mut hot = files(db);
    hot["path"] = json!(db);
    hot["page_size"] = json!(pragma("page_size")?);
    hot["pages"] = json!(pragma("page_count")?);
    hot["free_pages"] = json!(pragma("freelist_count")?);
    hot["auto_vacuum"] = json!(auto_vacuum);
    hot["kinds"] = kinds(&conn)?;
    hot["blobs"] = blobs(&conn)?;
    hot["control"] = json!(head);
    Ok(json!({
        "db": hot,
        "archive": archived,
        "checkpoints": checkpoints,
        "retention_days": s.retention.as_secs() / 86_400,
        "archive_on": s.archive,
        "next": {
            "rows": plan.picks.len(),
            "bytes": plan.bytes(),
            "purged_threads": plan.purged_threads,
            "turns": plan.turns,
            "kinds": next,
            "automatic": plan.picks.len() >= BATCH,
        },
    }))
}

/// Runs a compaction when one is due: first [`FIRST_LOOK`] after the hub opened (`every` when
/// that is shorter), then every `every`, until the hub stops.
pub(super) async fn prune_ticker(hub: Weak<Inner>, every: Duration) {
    let mut wait = every.min(FIRST_LOOK);
    loop {
        tokio::time::sleep(wait).await;
        wait = every;
        let Some(inner) = hub.upgrade() else {
            return;
        };
        if *inner.state.borrow() != HubState::Running {
            return;
        }
        let settings = Settings::of(&inner.config.log);
        let hub = Hub { inner };
        match hub.run_compaction(settings).await {
            Ok(r) if r.rows > 0 || r.blobs > 0 || r.freed_pages > 0 => {
                tracing::info!(
                    rows = r.rows,
                    bytes = r.bytes,
                    blobs = r.blobs,
                    blob_bytes = r.blob_bytes,
                    freed_pages = r.freed_pages,
                    archived = r.archived,
                    longest_batch_ms = r.longest_batch_ms,
                    ms = r.total_ms,
                    "compacted the log"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e.message, "cannot compact the log"),
        }
    }
}

impl Hub {
    /// A compaction with `s` of the hub's log, in a blocking task, under the hub's
    /// maintenance lock (never while a checkpoint is written).
    async fn run_compaction(&self, s: Settings) -> Result<Report, RpcError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _one = inner
                .maintenance
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            compact(&inner.db_path, &s, Some(&inner.core))
        })
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL, format!("the compaction failed: {e}")))?
        .map_err(|e| RpcError::new(codes::INTERNAL, format!("the compaction failed: {e:#}")))
    }

    /// `log.compact {vacuum?, drop_checkpoints?}` (users only): a compaction now, whatever
    /// how few rows it moves (see the module docs); with `vacuum`, then a `VACUUM` (the hub
    /// waits for it); with `drop_checkpoints`, the checkpoints dropped first. Returns the
    /// [`Report`].
    pub async fn log_compact(
        &self,
        vacuum: bool,
        drop_checkpoints: bool,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(RpcError::new(
                codes::UNAUTHORIZED,
                "only the user compacts the log",
            ));
        }
        let mut s = Settings::of(&self.inner.config.log);
        s.min_rows = 1;
        s.vacuum = vacuum;
        s.drop_checkpoints = drop_checkpoints;
        let r = self.run_compaction(s).await?;
        serde_json::to_value(r).map_err(|e| RpcError::new(codes::INTERNAL, e.to_string()))
    }

    /// `log.stats` (users only): the sizes of `twr.db` (its write-ahead log, pages, free
    /// pages, rows and bytes per kind, blobs), of `archive.db`, the checkpoints (age, size, how
    /// far behind the log), the control backlog (`control`: every item; `compacted`: what a
    /// compacted subscription from the start delivers) and what the next compaction moves
    /// (`next`).
    pub async fn log_stats(&self, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(RpcError::new(
                codes::UNAUTHORIZED,
                "only the user reads the log's statistics",
            ));
        }
        let (head, at_head) = {
            let core = self.inner.core();
            (core.gseq, twr_core::compact::Head::of(&core.registry))
        };
        let inner = self.inner.clone();
        let s = Settings::of(&inner.config.log);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
            let mut v = stats(&inner.db_path, &s)?;
            let conn = store::connect(&inner.db_path)?;
            let rows = store::read_control_rows(&conn, 0, head, usize::MAX)?;
            let kept = twr_core::compact::select(&rows, 0, &at_head).len();
            v["backlog"] = json!({ "head": head, "full": rows.len(), "compacted": kept });
            Ok(v)
        })
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL, format!("reading the log failed: {e}")))?
        .map_err(|e| RpcError::new(codes::INTERNAL, format!("reading the log failed: {e:#}")))
    }
}

/// The events of `thread` with `seq > since`, oldest first, at most `limit`, from `hot` and
/// the archive `archived` (when there is one), merged by seq: of an event both have, `hot`'s
/// ([`merge_archived`]).
#[cfg(test)]
pub(super) fn merged_thread_events(
    hot: &Connection,
    archived: Option<&Connection>,
    thread: &str,
    since: u64,
    limit: usize,
) -> anyhow::Result<Vec<Event>> {
    let read = store::read_thread_events(hot, thread, since, limit)?;
    merge_archived(read, archived, thread, since, limit)
}

/// `hot`, the events of `thread` with `seq > since` (at most `limit`) as `twr.db` had them,
/// merged by seq with those the archive `archived` has: of an event both have, `hot`'s; at
/// most `limit`. `hot` must be read first: a run copies a row into the archive before it
/// deletes it from `twr.db`, so a row `hot` missed is in the archive by the time it is read.
pub(super) fn merge_archived(
    mut hot: Vec<Event>,
    archived: Option<&Connection>,
    thread: &str,
    since: u64,
    limit: usize,
) -> anyhow::Result<Vec<Event>> {
    let Some(a) = archived else {
        return Ok(hot);
    };
    let old = store::read_thread_events(a, thread, since, limit)?;
    if old.is_empty() {
        return Ok(hot);
    }
    let have: HashSet<u64> = hot.iter().map(|e| e.seq).collect();
    hot.extend(old.into_iter().filter(|e| !have.contains(&e.seq)));
    hot.sort_by_key(|e| e.seq);
    hot.truncate(limit);
    Ok(hot)
}

/// Whether the archive at `path` has blob `blob_ref` (tests).
#[cfg(test)]
pub(super) fn archived_blob(path: &Path, blob_ref: &str) -> bool {
    archive::open_reader(path).is_some_and(|c| {
        c.query_row(
            "SELECT 1 FROM blobs WHERE hash = ?1",
            [blob_ref],
            |_| Ok(()),
        )
        .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;
    use twr_core::blob::{blob_ref, hydrate_with};
    use twr_core::config::NotifyConfig;
    use twr_core::event::*;
    use twr_core::testkit::{base_time, ThreadScript};

    use super::super::checkpoint::tests::{
        answers, check_log, control_ids, diff, first_difference, quiet_options, rich_log, settle,
        write_at, Log, ADAPTER,
    };
    use super::*;
    use crate::hub::checkpoint::tests::fold_dump;
    use crate::store::Store;

    /// A run's settings at `now`, with `retention`, moving any number of rows.
    fn settings(now: DateTime<Utc>, retention_days: u64, archive: bool) -> Settings {
        Settings {
            retention: Duration::from_secs(retention_days * 86_400),
            archive,
            now,
            min_rows: 1,
            vacuum: false,
            drop_checkpoints: false,
            batch: 3,
            budget: WRITE_BUDGET,
            pause: Duration::ZERO,
            on_mark: None,
            before_lock: None,
            between_copies: None,
        }
    }

    /// Long after every event of the made-up logs.
    fn later() -> DateTime<Utc> {
        base_time() + chrono::Duration::days(400)
    }

    /// `n` bytes of text that differ from any other `tag`'s (a payload past the cap moves to
    /// a blob).
    fn long(tag: &str, n: usize) -> String {
        format!("{tag}: {}", "x".repeat(n))
    }

    /// One worker turn of `thread`: what it said on the way, a tool call with a long input and
    /// its result (`output`), its final reply, its end (`result`), and a report unless
    /// `report` is false.
    fn turn(l: &mut Log, thread: &str, n: u64, result: TurnResult, report: bool, output: &str) {
        l.with(thread, ADAPTER(), |s| s.turn_started(&format!("do {n}")));
        l.ev(
            thread,
            ADAPTER(),
            EventBody::AgentThinking(AgentThinking {
                text: format!("thinking {n}"),
                ..Default::default()
            }),
        );
        l.ev(
            thread,
            ADAPTER(),
            EventBody::AgentText(AgentText {
                text: format!("on my way {n}"),
                ..Default::default()
            }),
        );
        l.ev(
            thread,
            ADAPTER(),
            EventBody::AgentToolCall(AgentToolCall {
                call_id: format!("c{n}"),
                tool: "Bash".into(),
                input_summary: long(&format!("{thread} input {n}"), 6000),
                ..Default::default()
            }),
        );
        l.ev(
            thread,
            ADAPTER(),
            EventBody::AgentToolResult(AgentToolResult {
                call_id: format!("c{n}"),
                ok: n.is_multiple_of(2),
                output_summary: output.to_string(),
                ..Default::default()
            }),
        );
        l.with(thread, ADAPTER(), |s| s.text(&format!("done {n}")));
        l.with(thread, ADAPTER(), |s| s.turn_ended(result));
        if report {
            l.with(thread, Source::Hub, |s| s.report(Some(n), Grade::Success));
            // In the turn, as the hub records it.
            l.events.last_mut().unwrap().turn = Some(n);
        }
    }

    /// Records the purge of trash item `item` of thread `thread` in the log at `db`, after
    /// what the hub's thread has.
    fn purge(db: &Path, thread: &str, item: &str) {
        let mut store = Store::open(db).unwrap();
        let seq = store::thread_head(store.conn(), HUB_THREAD).unwrap() + 1;
        let mut ev = Event::new(
            HUB_THREAD.to_string(),
            seq,
            None,
            Source::Hub,
            EventBody::TrashPurged(TrashPurged {
                item: item.into(),
                thread: thread.into(),
                ..Default::default()
            }),
        );
        ev.ts = later() - chrono::Duration::days(300);
        store
            .append(&ev, true, crate::store::AppendExtras::default())
            .unwrap();
    }

    /// A worker thread `thread` created on the local box.
    fn created(l: &mut Log, thread: &str, role: Option<&str>) {
        l.ev(
            thread,
            Source::Hub,
            EventBody::ThreadCreated(ThreadCreated {
                name: thread.trim_start_matches("t-").into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                ext: role.and_then(|r| super::super::checkpoint::tests::twr(json!({"role": r}))),
                ..Default::default()
            }),
        );
        l.with(thread, ADAPTER(), |s| s.started(&format!("s-{thread}")));
    }

    /// A deleted thread `thread` whose trash item `item` is purged when `purged`.
    fn deleted(l: &mut Log, thread: &str, item: &str, purged: bool, shared: &str) {
        created(l, thread, None);
        turn(l, thread, 1, TurnResult::Completed, true, shared);
        turn(l, thread, 2, TurnResult::Completed, true, "two");
        l.with(thread, Source::Hub, |s| s.parked());
        l.ev(
            thread,
            Source::Hub,
            EventBody::ThreadDeleted(ThreadDeleted {
                name: thread.trim_start_matches("t-").into(),
                trash: Some(item.into()),
                ..Default::default()
            }),
        );
        if purged {
            l.ev(
                HUB_THREAD,
                Source::Hub,
                EventBody::TrashPurged(TrashPurged {
                    item: item.into(),
                    thread: thread.into(),
                    ..Default::default()
                }),
            );
        }
    }

    /// The text moved rows and a kept one (the last turn's) share: one blob.
    fn shared() -> String {
        long("shared output", 5000)
    }

    /// A log with a worker of five turns (reported, unreported, interrupted, reported, and the
    /// last), an orchestrator (a turn that failed without its item, one that completed, the
    /// last), a deleted thread whose item is purged and one whose item is not. Returns the
    /// pointer of a blob only a kept report reaches, through which another blob, which a moved
    /// row points to as well, is reached.
    fn scenario(db: &Path) -> (String, String) {
        let mut l = Log::default();
        created(&mut l, "t-o", Some("orchestrator"));
        turn(&mut l, "t-o", 1, TurnResult::Error, false, "o1");
        turn(&mut l, "t-o", 2, TurnResult::Completed, false, "o2");
        turn(&mut l, "t-o", 3, TurnResult::Completed, false, "o3");
        created(&mut l, "t-k", None);
        turn(&mut l, "t-k", 1, TurnResult::Completed, true, &shared());
        turn(&mut l, "t-k", 2, TurnResult::Completed, false, "k2");
        turn(&mut l, "t-k", 3, TurnResult::Interrupted, false, "k3");
        turn(&mut l, "t-k", 4, TurnResult::Completed, true, &shared());
        // A blob in a blob: the report's `detail_ref` reaches `outer`, which holds `inner`'s
        // pointer; a moved row points to `inner` too.
        let inner_bytes = long("inner", 100);
        let inner = blob_ref(inner_bytes.as_bytes());
        let outer_bytes =
            serde_json::to_vec(&json!({"text": "", "text_ref": inner, "_capped": ["text"]}))
                .unwrap();
        let outer = blob_ref(&outer_bytes);
        l.with("t-k", Source::Hub, |s| s.report(Some(4), Grade::Success));
        let mut ev = l.events.pop().unwrap();
        ev.turn = Some(4);
        if let EventBody::ReportReady(r) = &mut ev.body {
            r.extra.insert("detail_ref".into(), json!(outer));
        }
        l.events.push(ev);
        // A late frame of turn 4 (after its end, before turn 5) points at `inner` too.
        l.ev(
            "t-k",
            ADAPTER(),
            EventBody::AgentText(AgentText {
                text: "points at inner".into(),
                extra: json!({ "note_ref": inner }).as_object().cloned().unwrap(),
                ..Default::default()
            }),
        );
        l.events.last_mut().unwrap().turn = Some(4);
        turn(&mut l, "t-k", 5, TurnResult::Completed, true, &shared());
        deleted(&mut l, "t-p", "01ITEMPURGED", true, &shared());
        deleted(&mut l, "t-d", "01ITEMKEPT", false, "d");
        l.write(db);
        let conn = store::connect(db).unwrap();
        for (hash, bytes) in [(&inner, inner_bytes.into_bytes()), (&outer, outer_bytes)] {
            conn.execute(
                "INSERT INTO blobs (hash, bytes) VALUES (?1, ?2)",
                params![hash, bytes],
            )
            .unwrap();
        }
        (outer, inner)
    }

    /// The `(thread, turn, kind)` of every agent event of the log at `db`.
    fn agent_rows(db: &Path) -> Vec<(String, Option<u64>, String, bool)> {
        let conn = store::connect(db).unwrap();
        store::load_with_gseq(&conn, None)
            .unwrap()
            .into_iter()
            .filter(|(e, _)| e.body.is_agent())
            .map(|(e, _)| {
                let fin = matches!(&e.body, EventBody::AgentText(t) if t.is_final);
                let kind = e.kind().to_string();
                (e.thread, e.turn, kind, fin)
            })
            .collect()
    }

    /// Every event of the log at `db` hydrates from its blobs (`archived`: or the archive's).
    fn hydrates(db: &Path, archived: bool) -> anyhow::Result<usize> {
        let conn = store::connect(db)?;
        let a = archived
            .then(|| archive::open_reader(&archive::path_of(db)))
            .flatten();
        let mut n = 0;
        for (ev, _) in store::load_with_gseq(&conn, None)? {
            let mut data = ev.body.data()?;
            let mut fetch = |r: &str| -> anyhow::Result<Option<Vec<u8>>> {
                match store::get_blob(&conn, r)? {
                    Some(b) => Ok(Some(b)),
                    None => a.as_ref().map_or(Ok(None), |a| store::get_blob(a, r)),
                }
            };
            n += hydrate_with(&mut data, &mut fetch, false)?;
            if let Some(r) = &ev.raw {
                fetch(r)?.ok_or_else(|| anyhow::anyhow!("raw {r} of {} is gone", ev.id))?;
            }
        }
        Ok(n)
    }

    /// What the hub reads of old turns, by store query: final texts, tool counts, the turns
    /// recovery completes, transcripts.
    fn reads(db: &Path) -> Value {
        let conn = store::connect(db).unwrap();
        let mut out = serde_json::Map::new();
        for t in ["t-o", "t-k", "t-d"] {
            for n in 1..=5 {
                let fin = store::last_final_text(&conn, t, n)
                    .unwrap()
                    .map(|e| (e.seq, e.id));
                out.insert(format!("{t}/{n}/final"), json!(fin));
            }
            out.insert(
                format!("{t}/latest"),
                json!(store::latest_final_text(&conn, t).unwrap().map(|e| e.id)),
            );
            // Each turn's transcript, but the tool calls and results of old turns (which
            // move: `twr events` reads them from the archive).
            for n in 1..=5 {
                let Ok((s, b, _)) = store::transcript_bounds(&conn, t, Some(n)) else {
                    continue;
                };
                let tr: Vec<String> = store::read_transcript_events(&conn, t, s, b, 500)
                    .unwrap()
                    .into_iter()
                    .filter(|e| !e.kind().starts_with("agent.tool"))
                    .map(|e| e.id)
                    .collect();
                out.insert(format!("{t}/{n}/transcript"), json!(tr));
            }
        }
        let workers: Vec<String> = conn
            .prepare("SELECT DISTINCT thread FROM events WHERE thread != 't-o' ORDER BY thread")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let unreported: Vec<String> = store::unreported_turn_ends(&conn, &workers)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        out.insert("unreported".into(), json!(unreported));
        out.insert(
            "t-k/2/tools".into(),
            json!(store::turn_tool_counts(&conn, "t-k", 2).unwrap()),
        );
        out.insert(
            "t-o/1/tools".into(),
            json!(store::turn_tool_counts(&conn, "t-o", 1).unwrap()),
        );
        out.insert(
            "failed".into(),
            json!(store::failed_turns_without_error_item(&conn, "t-o").unwrap()),
        );
        Value::Object(out)
    }

    /// What moves and what stays: all of a purged thread's agent events; of the others, a
    /// reported (or interrupted) worker turn's and a completed orchestrator turn's, but not
    /// their final replies, not a turn without its reactions (an unreported worker turn, a
    /// failed orchestrator turn without its item), not the last turn nor what came after its
    /// start (a deleted thread whose item is not purged is as any other). Nothing the hub
    /// reads changes, the
    /// fold is the same (and a checkpoint taken before stays valid), every row left hydrates,
    /// and a blob a kept row reaches (directly, or through another blob) stays.
    #[test]
    fn a_compaction_moves_only_what_nothing_reads() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let (outer, inner) = scenario(&db);
        let notify = NotifyConfig::default();
        let before_rows = agent_rows(&db);
        let before_reads = reads(&db);
        let full = fold_dump(Core::replay_full(&db, None).unwrap().0);
        let last = control_ids(&db).last().unwrap().clone();
        let cp = write_at(&db, &notify, &last).unwrap().unwrap();
        let input_blob = {
            let conn = store::connect(&db).unwrap();
            let ev = store::read_thread_events(&conn, "t-k", 0, 1000)
                .unwrap()
                .into_iter()
                .find(|e| matches!(&e.body, EventBody::AgentToolCall(c) if c.call_id == "c1"))
                .unwrap();
            ev.body.data().unwrap()["input_ref"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let r = compact(&db, &settings(later(), 7, true), None).unwrap();
        assert!(r.skipped.is_none(), "{r:?}");
        let after_rows = agent_rows(&db);
        let gone: Vec<_> = before_rows
            .iter()
            .filter(|row| !after_rows.contains(row))
            .collect();
        assert_eq!(r.rows, gone.len());
        // The purged thread: everything.
        assert!(!after_rows.iter().any(|(t, ..)| t == "t-p"));
        let moved = |t: &str, n: u64| {
            gone.iter()
                .filter(|(th, turn, ..)| th == t && *turn == Some(n))
                .count()
        };
        // Reported, interrupted, completed orchestrator turns: all but the final reply (and
        // a late frame of turn 4 with it).
        assert_eq!(moved("t-k", 1), 4, "{gone:?}");
        assert_eq!(moved("t-k", 3), 4);
        assert_eq!(moved("t-k", 4), 5);
        assert_eq!(moved("t-o", 2), 4);
        // A deleted thread whose item is not purged (it may be restored) as any other.
        assert_eq!(moved("t-d", 1), 4);
        // Without their reactions, the last turns, what came after the last turn's start, a
        // deleted thread whose item is not purged: nothing.
        for (t, n) in [("t-k", 2), ("t-o", 1), ("t-k", 5), ("t-o", 3), ("t-d", 2)] {
            assert_eq!(moved(t, n), 0, "{t} {n}");
        }
        assert!(gone.iter().all(|(t, _, _, fin)| t == "t-p" || !fin));
        assert_eq!(r.purged_threads, 1);
        assert_eq!(r.turns, 5);
        assert_eq!(reads(&db), before_reads);
        let after = fold_dump(Core::replay_full(&db, None).unwrap().0);
        assert!(after == full, "{}", diff(&after, &full));
        let (core, _) = Core::replay_with(&db, None).unwrap();
        assert_eq!(core.replayed.checkpoint, Some(cp.id));
        let from_cp = fold_dump(core);
        assert!(from_cp == full, "{}", diff(&from_cp, &full));
        let ids = control_ids(&db);
        check_log(&db, &notify, &ids[ids.len() / 2..]).unwrap();
        // Blobs: the rows left hydrate from twr.db alone, the moved ones from the archive.
        hydrates(&db, false).unwrap();
        let conn = store::connect(&db).unwrap();
        let hot = |r: &str| store::get_blob(&conn, r).unwrap().is_some();
        assert!(hot(&outer) && hot(&inner), "reached from a kept report");
        assert!(
            hot(&blob_ref(shared().as_bytes())),
            "shared with a kept row"
        );
        assert!(!hot(&input_blob), "only a moved row reached it");
        let apath = archive::path_of(&db);
        assert!(archived_blob(&apath, &input_blob));
        // The archive holds every blob its rows reach, those still in twr.db too.
        for r in [&inner, &blob_ref(shared().as_bytes())] {
            assert!(archived_blob(&apath, r), "{r}");
        }
        assert!(r.blobs > 0 && r.blob_bytes > 0, "{r:?}");
        let a = archive::open_reader(&apath).unwrap();
        let archived: i64 = a
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(archived as usize, r.rows);
        hydrates(&db, true).unwrap();
        // Every moved row reads back merged with the rest, in order, nothing twice.
        for t in ["t-k", "t-p", "t-o"] {
            let all = merged_thread_events(&conn, Some(&a), t, 0, 10_000).unwrap();
            let seqs: Vec<u64> = all.iter().map(|e| e.seq).collect();
            assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>(), "{t}");
            // And pages of it too.
            let page = merged_thread_events(&conn, Some(&a), t, 3, 4).unwrap();
            assert_eq!(
                page.iter().map(|e| e.seq).collect::<Vec<_>>(),
                [4, 5, 6, 7],
                "{t}"
            );
        }
        // Again: nothing more to move.
        let again = compact(&db, &settings(later(), 7, true), None).unwrap();
        assert_eq!((again.rows, again.blobs), (0, 0), "{again:?}");
    }

    /// The age and outbox rules: a turn that ended within `retention_days`, or one a recorded
    /// command for a remote box still names, stays; an automatic run moves nothing below a
    /// batch.
    #[test]
    fn recent_turns_and_turns_of_recorded_commands_stay() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        scenario(&db);
        let conn = store::connect(&db).unwrap();
        let ended = |t: &str, n: u64| -> DateTime<Utc> {
            store::last_of_kind(&conn, t, Some(n), "turn.ended")
                .unwrap()
                .unwrap()
                .ts
        };
        // Seven days after turn 3 of t-k ended: turn 1 and the orchestrator's turn 2 (which
        // ended before it) are old enough, turn 3 is not.
        let now = ended("t-k", 3) + chrono::Duration::days(7);
        let p = plan(&conn, &settings(now, 7, true)).unwrap();
        assert_eq!(p.turns, 2, "{p:?}");
        let p = plan(
            &conn,
            &settings(now + chrono::Duration::seconds(1), 7, true),
        )
        .unwrap();
        assert_eq!(p.turns, 3);
        conn.execute(
            "INSERT INTO remote_outbox (box, cmd_id, thread, turn, op, created)
             VALUES ('gpu', 'send:t-k:1', 't-k', 1, '{}', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let p = plan(&conn, &settings(later(), 7, true)).unwrap();
        assert_eq!(p.turns, 4, "t-k turn 1 stays while its command is recorded");
        let mut auto = settings(later(), 7, true);
        auto.min_rows = BATCH;
        let r = compact(&db, &auto, None).unwrap();
        assert_eq!(r.rows, 0);
        assert!(r.skipped.unwrap().contains("fewer than"));
    }

    /// An old turn's agent event recorded after the start of the thread's last turn (a late
    /// frame) stays: what the thread did last is what its latest events say.
    #[test]
    fn a_late_frame_after_the_last_turn_began_stays() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let mut l = Log::default();
        created(&mut l, "t-z", None);
        turn(&mut l, "t-z", 1, TurnResult::Completed, true, "z1");
        l.with("t-z", ADAPTER(), |s| s.turn_started("again"));
        l.ev(
            "t-z",
            ADAPTER(),
            EventBody::AgentText(AgentText {
                text: "late".into(),
                ..Default::default()
            }),
        );
        l.events.last_mut().unwrap().turn = Some(1);
        l.write(&db);
        let full = fold_dump(Core::replay_full(&db, None).unwrap().0);
        let r = compact(&db, &settings(later(), 7, true), None).unwrap();
        assert_eq!(r.rows, 4, "{r:?}");
        let late = agent_rows(&db)
            .into_iter()
            .filter(|(_, turn, _, fin)| *turn == Some(1) && !fin)
            .count();
        assert_eq!(late, 1);
        let after = fold_dump(Core::replay_full(&db, None).unwrap().0);
        assert!(after == full, "{}", diff(&after, &full));
    }

    /// A prompt queued between a blob deletion's reads and its write lock is seen under the
    /// lock (the queue moved): nothing is deleted then, and the blob it points to stays.
    #[test]
    fn a_pointer_queued_just_before_the_lock_keeps_its_blob() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        scenario(&db);
        let input_blob = first_input(&db);
        PENDING_REF.with(|p| *p.borrow_mut() = input_blob.clone());
        let mut s = settings(later(), 7, false);
        s.before_lock = Some(|conn: &Connection| {
            let r = PENDING_REF.with(|p| std::mem::take(&mut *p.borrow_mut()));
            if !r.is_empty() {
                conn.execute(
                    "INSERT INTO pending_prompts (thread, prompt, from_, source, queued_at)
                     VALUES ('t-k', ?1, 'user', 'user:tui', '2026-01-01T00:00:00Z')",
                    [format!("look at {r}")],
                )
                .unwrap();
            }
        });
        let r = compact(&db, &s, None).unwrap();
        assert!(r.blobs > 0, "{r:?}");
        let conn = store::connect(&db).unwrap();
        assert!(store::get_blob(&conn, &input_blob).unwrap().is_some());
    }

    /// The pointer of t-k's first tool call's input (a blob only moved rows reach).
    fn first_input(db: &Path) -> String {
        let conn = store::connect(db).unwrap();
        let ev = store::read_thread_events(&conn, "t-k", 0, 1000)
            .unwrap()
            .into_iter()
            .find(|e| matches!(&e.body, EventBody::AgentToolCall(c) if c.call_id == "c1"))
            .unwrap();
        ev.body.data().unwrap()["input_ref"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A run that dies while it copies leaves no archived row whose blobs the archive lacks:
    /// the blobs go in first. Then, without an archive, the archived rows still read whole.
    #[test]
    fn a_crash_while_copying_leaves_no_row_without_its_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        scenario(&db);
        let mut s = settings(later(), 7, true);
        s.between_copies = Some(|_: &Connection| anyhow::bail!("crash"));
        assert!(compact(&db, &s, None).is_err());
        let check = || {
            let a = archive::open_reader(&archive::path_of(&db)).unwrap();
            for t in ["t-k", "t-p", "t-o", "t-d"] {
                for ev in store::read_thread_events(&a, t, 0, 10_000).unwrap() {
                    let mut data = ev.body.data().unwrap();
                    hydrate_with(&mut data, &mut |r: &str| store::get_blob(&a, r), false)
                        .unwrap_or_else(|e| panic!("{}: {e:#}", ev.id));
                }
            }
        };
        check();
        // The next run, without an archive, moves the rest: still whole.
        let r = compact(&db, &settings(later(), 7, false), None).unwrap();
        assert!(r.rows > 0, "{r:?}");
        check();
    }

    /// A blob deletion takes at most 8 MB in one transaction: two 5 MB blobs go in two, and a
    /// blob larger than that alone.
    #[test]
    fn a_transaction_deletes_at_most_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let store = Store::open(&db).unwrap();
        for tag in ["a", "b"] {
            let bytes = long(tag, 5 << 20).into_bytes();
            store
                .conn()
                .execute(
                    "INSERT INTO blobs (hash, bytes) VALUES (?1, ?2)",
                    params![blob_ref(&bytes), bytes],
                )
                .unwrap();
        }
        drop(store);
        let r = compact(&db, &settings(later(), 7, false), None).unwrap();
        assert_eq!((r.blobs, r.batches), (2, 2), "{r:?}");
    }

    /// A prompt queued after the blobs were marked (with no event) keeps a blob it points to;
    /// with `[log] archive = false` it would be lost for good otherwise.
    #[test]
    fn a_pointer_queued_during_a_run_keeps_its_blob() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        scenario(&db);
        let mut s = settings(later(), 7, false);
        s.on_mark = Some(|conn: &Connection| {
            // The input of t-k's first tool call, which only moved rows pointed to.
            let r = PENDING_REF.with(|p| p.borrow().clone());
            conn.execute(
                "INSERT INTO pending_prompts (thread, prompt, from_, source, queued_at)
                 VALUES ('t-k', ?1, 'user', 'user:tui', '2026-01-01T00:00:00Z')",
                [format!("look at {r}")],
            )
            .unwrap();
        });
        let input_blob = {
            let conn = store::connect(&db).unwrap();
            let ev = store::read_thread_events(&conn, "t-k", 0, 1000)
                .unwrap()
                .into_iter()
                .find(|e| matches!(&e.body, EventBody::AgentToolCall(c) if c.call_id == "c1"))
                .unwrap();
            ev.body.data().unwrap()["input_ref"]
                .as_str()
                .unwrap()
                .to_string()
        };
        PENDING_REF.with(|p| *p.borrow_mut() = input_blob.clone());
        let r = compact(&db, &s, None).unwrap();
        assert!(r.blobs > 0, "{r:?}");
        let conn = store::connect(&db).unwrap();
        assert!(store::get_blob(&conn, &input_blob).unwrap().is_some());
    }

    thread_local! {
        /// The pointer [`a_pointer_queued_during_a_run_keeps_its_blob`] queues.
        static PENDING_REF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }

    /// Archived rows never need `twr.db`: a blob they share with a row that stays is copied
    /// too, so once that row goes as well, without an archive (`archive = false`), the
    /// archived rows still read whole.
    #[test]
    fn archived_rows_keep_their_blobs_after_the_archive_is_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        scenario(&db);
        compact(&db, &settings(later(), 7, true), None).unwrap();
        let conn = store::connect(&db).unwrap();
        let shared_ref = blob_ref(shared().as_bytes());
        assert!(store::get_blob(&conn, &shared_ref).unwrap().is_some());
        // A sixth turn: the fifth (which shares the blob) can go, without an archive.
        let mut l = Log::default();
        let seq = store::thread_head(&conn, "t-k").unwrap();
        turn(&mut l, "t-k", 6, TurnResult::Completed, true, "k6");
        for ev in &mut l.events {
            ev.seq += seq;
            ev.turn = ev.turn.map(|_| 6);
            ev.ts = later() - chrono::Duration::days(100);
        }
        l.write(&db);
        let r = compact(&db, &settings(later(), 7, false), None).unwrap();
        assert!(r.rows > 0, "{r:?}");
        assert!(store::get_blob(&conn, &shared_ref).unwrap().is_none());
        hydrates(&db, true).unwrap();
        let a = archive::open_reader(&archive::path_of(&db)).unwrap();
        for ev in store::read_thread_events(&a, "t-k", 0, 10_000).unwrap() {
            let mut data = ev.body.data().unwrap();
            hydrate_with(&mut data, &mut |r: &str| store::get_blob(&a, r), false)
                .unwrap_or_else(|e| panic!("{}: {e:#}", ev.id));
        }
    }

    /// With `[log] archive = false` the rows and blobs go, and nothing is made.
    #[test]
    fn without_an_archive_they_are_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        scenario(&db);
        let full = fold_dump(Core::replay_full(&db, None).unwrap().0);
        let r = compact(&db, &settings(later(), 7, false), None).unwrap();
        assert!(r.rows > 0 && r.blobs > 0, "{r:?}");
        assert!(!r.archived);
        assert!(!archive::path_of(&db).exists());
        hydrates(&db, false).unwrap();
        let after = fold_dump(Core::replay_full(&db, None).unwrap().0);
        assert!(after == full, "{}", diff(&after, &full));
    }

    /// A run that copied its rows and died before deleting them leaves them in both
    /// databases: readers take twr.db's, and the next run deletes them (once).
    #[test]
    fn a_crash_between_copy_and_delete_is_finished_by_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        scenario(&db);
        let s = settings(later(), 7, true);
        // The copy, as a run makes it, and no delete.
        let n = {
            let mut conn = store::connect(&db).unwrap();
            let p = plan(&conn, &s).unwrap();
            drop(archive::open(&archive::path_of(&db)).unwrap());
            conn.execute(
                "ATTACH DATABASE ?1 AS archive",
                [archive::path_of(&db).to_string_lossy()],
            )
            .unwrap();
            // (The deletes are what the crash skips.)
            let tx = conn.transaction().unwrap();
            let rowids: Vec<i64> = p.picks.iter().map(|p| p.rowid).collect();
            tx.execute(
                "INSERT OR IGNORE INTO archive.events
                 SELECT thread, seq, id, v, ts, turn, source, kind, data, raw FROM main.events
                 WHERE rowid IN (SELECT value FROM json_each(?1))",
                [serde_json::to_string(&rowids).unwrap()],
            )
            .unwrap();
            tx.commit().unwrap();
            p.picks.len()
        };
        assert!(n > 0);
        // Both have them: a reader takes twr.db's (the archive's copy changed to tell).
        let a = archive::open(&archive::path_of(&db)).unwrap();
        a.execute(
            "UPDATE events SET data = json_set(data, '$.text', 'archived copy')
             WHERE kind = 'agent.text'",
            [],
        )
        .unwrap();
        let conn = store::connect(&db).unwrap();
        let merged = merged_thread_events(&conn, Some(&a), "t-k", 0, 10_000).unwrap();
        assert!(merged
            .iter()
            .all(|e| !matches!(&e.body, EventBody::AgentText(t) if t.text == "archived copy")));
        let hot = store::read_thread_events(&conn, "t-k", 0, 10_000).unwrap();
        assert_eq!(merged.len(), hot.len());
        let r = compact(&db, &s, None).unwrap();
        assert_eq!(r.rows, n);
        let archived: i64 = a
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(archived as usize, n);
        let again = compact(&db, &s, None).unwrap();
        assert_eq!(again.rows, 0);
        // A run cut short after its rows left blobs no row reaches: a run by hand moves them
        // with no row to move; an automatic one, which moves nothing, leaves them.
        let orphan = long("orphan", 10);
        let r = blob_ref(orphan.as_bytes());
        conn.execute(
            "INSERT INTO blobs (hash, bytes) VALUES (?1, ?2)",
            params![r, orphan.as_bytes()],
        )
        .unwrap();
        let mut auto = s.clone();
        auto.min_rows = BATCH;
        assert_eq!(compact(&db, &auto, None).unwrap().blobs, 0);
        let swept = compact(&db, &s, None).unwrap();
        assert_eq!((swept.rows, swept.blobs), (0, 1), "{swept:?}");
        assert!(archived_blob(&archive::path_of(&db), &r));
    }

    /// `VACUUM` (`--vacuum`) keeps the rows in their order across the gaps a compaction left,
    /// a checkpoint written before it is still used after it (its last event looked up by
    /// id), and incremental vacuum is on from then.
    #[test]
    fn vacuum_keeps_the_log_in_order_and_checkpoints_valid() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, true);
        let mut l = Log::default();
        created(&mut l, "t-x", None);
        for n in 1..=4 {
            turn(
                &mut l,
                "t-x",
                n,
                TurnResult::Completed,
                true,
                &long("x", 9000),
            );
        }
        l.write(&db);
        let notify = NotifyConfig::default();
        compact(&db, &settings(later(), 7, true), None).unwrap();
        let order = |db: &Path| -> Vec<String> {
            let conn = store::connect(db).unwrap();
            let mut st = conn
                .prepare("SELECT id FROM events ORDER BY rowid")
                .unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let rowids = |db: &Path| -> Vec<i64> {
            let conn = store::connect(db).unwrap();
            let mut st = conn
                .prepare("SELECT rowid FROM events ORDER BY rowid")
                .unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let before = order(&db);
        let gaps = rowids(&db).windows(2).filter(|w| w[1] != w[0] + 1).count();
        assert!(gaps > 0, "the compaction left gaps");
        let ids = control_ids(&db);
        let cp = write_at(&db, &notify, &ids[ids.len() / 2])
            .unwrap()
            .unwrap();
        let full = fold_dump(Core::replay_full(&db, None).unwrap().0);
        let mut s = settings(later(), 7, true);
        s.vacuum = true;
        let r = compact(&db, &s, None).unwrap();
        let v = r.vacuum.unwrap();
        assert!(v.incremental_set && v.after <= v.before, "{v:?}");
        assert_eq!(order(&db), before);
        let conn = store::connect(&db).unwrap();
        let mode: i64 = conn
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, 2);
        let (core, _) = Core::replay_with(&db, None).unwrap();
        assert_eq!(core.replayed.checkpoint, Some(cp.id));
        let got = fold_dump(core);
        assert!(got == full, "{}", diff(&got, &full));
        // Later runs give free pages back a step at a time.
        let mut l = Log::default();
        created(&mut l, "t-y", None);
        for n in 1..=3 {
            turn(
                &mut l,
                "t-y",
                n,
                TurnResult::Completed,
                true,
                &long("y", 20_000),
            );
        }
        l.write(&db);
        let r = compact(&db, &settings(later(), 7, true), None).unwrap();
        assert!(r.rows > 0);
        assert!(r.freed_pages > 0, "{r:?}");
    }

    /// A purge folds into the trash and the deleted thread's bookkeeping, on replay too: its
    /// item is no longer listed, the thread is not restorable, and what its agent did is not
    /// kept; an item of another deletion, or another thread's, changes nothing.
    #[test]
    fn a_purge_is_folded() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        scenario(&db);
        let (core, _) = Core::replay_full(&db, None).unwrap();
        assert!(!core.trash.contains_key("01ITEMPURGED"));
        assert!(core.trash.contains_key("01ITEMKEPT"));
        assert!(core.deleted_rt["t-p"].purged);
        assert!(!core.deleted_rt["t-d"].purged);
        assert!(!core.activity.contains_key("t-p"));
        assert!(core.activity.contains_key("t-d"));
        // A purge naming the item of another thread, or from someone else, is not folded.
        let mut s = ThreadScript::new(HUB_THREAD);
        let mut store = Store::open(&db).unwrap();
        for (thread, source) in [("t-o", Source::Hub), ("t-d", Source::User("tui".into()))] {
            let mut ev = s.event(EventBody::TrashPurged(TrashPurged {
                item: "01ITEMKEPT".into(),
                thread: thread.into(),
                ..Default::default()
            }));
            ev.seq += 100;
            ev.source = source;
            store
                .append(&ev, true, crate::store::AppendExtras::default())
                .unwrap();
        }
        drop(store);
        let (core, _) = Core::replay_full(&db, None).unwrap();
        assert!(!core.deleted_rt["t-d"].purged);
        assert!(core.trash.contains_key("01ITEMKEPT"));
        // The plan matches a purge by its item and thread, as the fold does.
        let conn = store::connect(&db).unwrap();
        let p = plan(&conn, &settings(later(), 7, true)).unwrap();
        assert_eq!(p.purged_threads, 1, "only t-p");
    }

    /// A hub opened on a compacted log answers clients and the orchestrator as before; it
    /// reads a purged thread's events by its id, those archived included.
    #[tokio::test]
    async fn a_hub_on_a_compacted_log_answers_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        rich_log(&db, true);
        let mut l = Log::default();
        deleted(&mut l, "t-p", "01ITEMPURGED", false, &shared());
        l.write(&db);
        purge(&db, "t-p", "01ITEMPURGED");
        settle(&db).await;
        let hub = Hub::open(quiet_options(&db)).await.unwrap();
        let before = answers(&hub);
        let events = |hub: &Hub| -> Vec<u64> {
            let (id, evs) = hub.thread_events("t-p", 0, 10_000).unwrap();
            assert_eq!(id, "t-p");
            evs.iter().map(|e| e.seq).collect()
        };
        let all = events(&hub);
        hub.shutdown().await;
        drop(hub);
        let r = compact(&db, &settings(later(), 7, true), None).unwrap();
        assert!(r.rows > 0 && r.purged_threads == 1, "{r:?}");
        let hub = Hub::open(quiet_options(&db)).await.unwrap();
        let after = answers(&hub);
        assert!(
            before == after,
            "{}",
            first_difference(&before, &after, "").unwrap_or_default()
        );
        assert_eq!(events(&hub), all);
        // `twr log compact` asks the running hub: the orchestrator may not.
        let e = hub
            .log_compact(false, false, &Caller::Orchestrator)
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::UNAUTHORIZED);
        let r = hub
            .log_compact(true, true, &Caller::user("test"))
            .await
            .unwrap();
        assert_eq!(r["rows"], 0, "{r}");
        assert!(r["vacuum"]["ms"].is_u64(), "{r}");
        let st = hub.log_stats(&Caller::user("test")).await.unwrap();
        assert!(
            st["db"]["kinds"]["thread.created"]["rows"]
                .as_u64()
                .unwrap()
                > 0,
            "{st}"
        );
        assert!(
            st["archive"]["kinds"]["agent.tool_call"]["rows"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(st["backlog"]["head"], st["db"]["control"]);
        assert!(st["backlog"]["compacted"].as_u64() <= st["backlog"]["full"].as_u64());
        assert_eq!(st["next"]["rows"], 0);
        hub.shutdown().await;
    }
}

/// A compaction of a real log, measured: `TWR_PRUNE_DB` names a copy of a real `twr.db` (it is
/// written), `TWR_PRUNE_TRASH` a file listing the trash items its box still has (one per line:
/// `ls <data>/trash`), `TWR_PRUNE_NOW` the time to compact at (RFC 3339; default: its last
/// event's). First the purges the hub would find (`gone`: deleted more than `[trash]
/// keep_days` before, and not in the listing) are recorded, then it is compacted at the default
/// `[log]` (30 days), then at the floor (7 days), then vacuumed; the fold is compared with the
/// one before after each step. Run in release mode: `cargo test --release -p twr-hub --lib --
/// --ignored --nocapture compaction_of_a_real_log`.
#[cfg(test)]
mod real {
    use super::*;
    use crate::hub::checkpoint::tests::{diff, fold_dump};
    use crate::store::Store;

    #[test]
    #[ignore = "needs a copy of a real log: TWR_PRUNE_DB, TWR_PRUNE_TRASH"]
    fn compaction_of_a_real_log() {
        let db = PathBuf::from(std::env::var_os("TWR_PRUNE_DB").expect("TWR_PRUNE_DB"));
        let listing: HashSet<String> =
            std::fs::read_to_string(std::env::var_os("TWR_PRUNE_TRASH").expect("TWR_PRUNE_TRASH"))
                .unwrap()
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
        let now = match std::env::var("TWR_PRUNE_NOW") {
            Ok(t) => DateTime::parse_from_rfc3339(&t)
                .unwrap()
                .with_timezone(&Utc),
            Err(_) => {
                let conn = store::connect(&db).unwrap();
                let ts: String = conn
                    .query_row("SELECT MAX(ts) FROM events", [], |r| r.get(0))
                    .unwrap();
                DateTime::parse_from_rfc3339(&ts)
                    .unwrap()
                    .with_timezone(&Utc)
            }
        };
        // (Migrated as a hub opening it would.)
        drop(Store::open(&db).unwrap());
        let size = |p: &Path| file_size(p) + file_size(&with_suffix(p, "-wal"));
        let show = |label: &str| {
            let s = stats(&db, &Settings::of(&Default::default())).unwrap();
            eprintln!(
                "{label}: twr.db {} B (wal {}), free pages {}, blobs {} ({} B), archive {} B",
                s["db"]["bytes"],
                s["db"]["wal"],
                s["db"]["free_pages"],
                s["db"]["blobs"]["count"],
                s["db"]["blobs"]["bytes"],
                s["archive"]["bytes"]
            );
            s
        };
        let fold = || {
            let t = Instant::now();
            let d = fold_dump(Core::replay_full(&db, None).unwrap().0);
            eprintln!("  (whole fold {} ms)", t.elapsed().as_millis());
            d
        };
        show("before");
        // The purges the hub's first purge round records: the items it knows past their
        // purge time that the box does not have.
        let keep_ms = twr_core::config::TrashConfig::default().keep_ms();
        let cutoff = u64::try_from(now.timestamp_millis()).unwrap() - keep_ms;
        let gone: Vec<(String, String)> = {
            let (core, _) = Core::replay_full(&db, None).unwrap();
            core.trash
                .iter()
                .filter(|(item, seen)| !listing.contains(*item) && seen.deleted_at_ms < cutoff)
                .map(|(item, seen)| (item.clone(), seen.thread.clone()))
                .collect()
        };
        {
            let mut store = Store::open(&db).unwrap();
            for (item, thread) in &gone {
                let seq = store::thread_head(store.conn(), HUB_THREAD).unwrap() + 1;
                let mut ev = Event::new(
                    HUB_THREAD.to_string(),
                    seq,
                    None,
                    twr_core::event::Source::Hub,
                    twr_core::event::EventBody::TrashPurged(twr_core::event::TrashPurged {
                        item: item.clone(),
                        thread: thread.clone(),
                        gone: true,
                        ..Default::default()
                    }),
                );
                ev.ts = now;
                store
                    .append(&ev, true, crate::store::AppendExtras::default())
                    .unwrap();
            }
        }
        eprintln!(
            "recorded {} purges (of {} items the log knows; {} listed)",
            gone.len(),
            {
                let (core, _) = Core::replay_full(&db, None).unwrap();
                core.trash.len() + gone.len()
            },
            listing.len()
        );
        let full = fold();
        let mut s = Settings::of(&Default::default());
        s.now = now;
        s.min_rows = 1;
        for days in [30, 7] {
            s.retention = Duration::from_secs(days * 86_400);
            let before = size(&db);
            let r = compact(&db, &s, None).unwrap();
            eprintln!(
                "retention {days} days: {}",
                serde_json::to_string(&r).unwrap()
            );
            eprintln!("  twr.db {before} -> {} B", size(&db));
            let after = fold();
            assert!(after == full, "{}", diff(&after, &full));
            show("  after");
        }
        s.vacuum = true;
        let r = compact(&db, &s, None).unwrap();
        eprintln!("vacuum: {}", serde_json::to_string(&r.vacuum).unwrap());
        let after = fold();
        assert!(after == full, "{}", diff(&after, &full));
        let st = show("after vacuum");
        eprintln!("{}", serde_json::to_string_pretty(&st).unwrap());
    }
}
