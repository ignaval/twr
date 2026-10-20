//! SQLite persistence: the append-only event log, the control index, blobs and delivery
//! bookkeeping.
//!
//! Tables (schema version [`SCHEMA_VERSION`], kept in `PRAGMA user_version`):
//!
//! - `events(thread, seq, id, v, ts, turn, source, kind, data, raw)`: one logical append-only
//!   log per thread, primary key `(thread, seq)`. Triggers abort any UPDATE or DELETE.
//! - `control(gseq, thread, seq)`: the hub control stream, a materialized index over `events`
//!   with its own global sequence. Also append-only (triggers).
//! - `blobs(hash, bytes)`: content-addressed payload overflow and raw runtime messages.
//! - `pending_prompts`: prompts accepted by `thread.send` but not yet dispatched as a turn
//!   (delivery bookkeeping). `turn.started` is appended in the same transaction
//!   that removes the row. `said_after` is the control head when the prompt arrived (schema
//!   2); the turn records it as `ext.twr.said_after` for the relay guard (`hub::orch`). The row
//!   id is the message's ref (`thread.send` / `say` return it, the turn records it as
//!   `ext.twr.ref`): AUTOINCREMENT, so never reused. `message.withdraw` removes a row in the
//!   same transaction as its `prompt.withdrawn`. `user_sent` (schema 8) is the control head
//!   after the batch that queued the row when the row is the user's own send (`thread.send`,
//!   `say`, an answer that routes the user's message), and NULL for every other prompt (a
//!   job's wake, the orchestrator's send, a prompt that waited for other threads): only the
//!   user's send releases a queue a crash held, also at the next hub start
//!   ([`Store::user_sent_since`]). Rows from before schema 8 have none.
//! - `runner_acks(box, thread, rseq)`: runner delivery watermarks: the highest
//!   runner sequence number of each (box, thread) whose frame is committed. Updated in the same
//!   transaction as the events a runner frame produced ([`AppendBatch::runner_ack`]), and
//!   only ever raised. That transaction is synced to disk before the hub acks the
//!   frame (the runner deletes an acked frame for good): either as it commits (`synchronous =
//!   FULL`), or, for a batch of frames of one connection, once for the whole batch
//!   ([`Store::sync`]) before any of them is acked.
//! - `remote_outbox(box, cmd_id, thread, turn, op, created)` (schema 3): the session commands
//!   (`send`, `respond`) a remote box still has to get. Recorded in the same transaction as the
//!   state change that implies them ([`AppendBatch::remote_cmds`]: `turn.started`,
//!   `attention.resolved`), so a hub that dies before the command reaches the runner sends it
//!   again, with the same id, once the box is back (the runner deduplicates ids). Deleted once
//!   the runner answers it, or when it no longer applies (`hub::runners`). One older than the
//!   runner keeps command results ([`OUTBOX_RETENTION`]: sending it again could run it twice)
//!   is never sent again, and is deleted in the same transaction as the `turn.ended` `error`
//!   its expiry means ([`AppendBatch::remote_done`]): until that commits it stays, so its turn
//!   is still ended later. A `stop` is never dropped this way: while its runtime lives it is
//!   sent again (running a stop twice is harmless).
//! - `stop_requests(thread, ts)` (schema 4): a `thread.stop` the hub has asked a runtime for
//!   (or a stop the hub decided on when a remote runtime's `send` / `respond` failed or
//!   expired, [`AppendBatch::stop_requests`]) and whose closing sequence is not recorded yet.
//!   A hub that restarts (or takes a remote session over again) closes such a runtime as
//!   stopped, not crashed, and asks it to stop again. Deleted in the transaction that records the thread's `thread.stopped` /
//!   `thread.crashed` / `thread.parked` / `thread.attached`.
//! - `spawn_intents(thread, min_start, ts)` (schema 4): a local runtime being started: recorded
//!   before the process exists, so a hub that dies before the runtime reported its pid can
//!   still find it at startup (the processes marked for the thread that started after
//!   `min_start`, [`twr_runner::reap::reap_marked`]). Deleted with the thread's closing events,
//!   when the start fails, and by the startup that reaped it.
//! - `attach_processes(thread, pid, start_time, marker, dir, ts)` (schema 5): the interactive
//!   process of the thread's attach, as its client reported it (`attach.child`): while it runs
//!   the thread stays attached even when the lease connection is gone, also across a hub
//!   restart (`hub`, Attach). Deleted in the transaction that records `thread.detached` (and
//!   `thread.attached`: a new attach has no process yet).
//!
//! Schema 7 changes no table: from it, `events` may hold `turn.started` `from: runtime`, which
//! a twr before it cannot decode. Such a twr refuses the database as newer than itself rather
//! than failing on those rows. Schema 8 adds `pending_prompts.user_sent`.
//!
//! Every append (event row, control row, blobs, dequeued prompt) is one transaction, and so is
//! every [`AppendBatch`]: several events plus queued prompts that make up one logical operation.

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use twr_core::blob::{blob_ref, BlobStore};
use twr_core::event::{AttentionReason, DecisionSite, EventBody, Grade, TurnFrom};
use twr_core::{Event, Source};

/// Current schema version (`PRAGMA user_version`).
pub const SCHEMA_VERSION: i32 = 8;

const SCHEMA_6: &str = r#"
CREATE TABLE IF NOT EXISTS spawn_operations (
    operation_id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    request TEXT NOT NULL,
    result TEXT,
    created TEXT NOT NULL
);
"#;

/// How long a recorded command for a remote box may wait to be sent again
/// ([`twr_core::runner::RESEND_HORIZON`]): the runner keeps command results (which make a
/// repeated command a no-op) a day longer, so an older command could run twice. Older records
/// are dropped (`hub::runners`).
pub const OUTBOX_RETENTION: Duration = twr_core::runner::RESEND_HORIZON;

/// Schema 4: durable stop requests, local spawn intents, and an index for the per-kind scans
/// (turn reactions, startup recovery).
const SCHEMA_4: &str = r#"
CREATE TABLE IF NOT EXISTS stop_requests (
    thread TEXT PRIMARY KEY,
    ts     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS spawn_intents (
    thread    TEXT PRIMARY KEY,
    min_start INTEGER NOT NULL,
    ts        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_kind_turn ON events (kind, thread, turn);
"#;

/// Schema 5: the interactive processes of attaches.
const SCHEMA_5: &str = r#"
CREATE TABLE IF NOT EXISTS attach_processes (
    thread     TEXT PRIMARY KEY,
    pid        INTEGER NOT NULL,
    start_time INTEGER NOT NULL,
    marker     TEXT NOT NULL,
    dir        TEXT,
    ts         TEXT NOT NULL
);
"#;

/// Schema 3: the outbox of commands for remote boxes.
const REMOTE_OUTBOX: &str = r#"
CREATE TABLE IF NOT EXISTS remote_outbox (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    box     TEXT NOT NULL,
    cmd_id  TEXT NOT NULL,
    thread  TEXT NOT NULL,
    turn    INTEGER,
    op      TEXT NOT NULL,
    created TEXT NOT NULL,
    UNIQUE (box, cmd_id)
);
"#;

const SCHEMA: &str = r#"
CREATE TABLE events (
    thread TEXT NOT NULL,
    seq    INTEGER NOT NULL,
    id     TEXT NOT NULL UNIQUE,
    v      INTEGER NOT NULL,
    ts     TEXT NOT NULL,
    turn   INTEGER,
    source TEXT NOT NULL,
    kind   TEXT NOT NULL,
    data   TEXT NOT NULL,
    raw    TEXT,
    PRIMARY KEY (thread, seq)
);
CREATE TABLE control (
    gseq   INTEGER PRIMARY KEY AUTOINCREMENT,
    thread TEXT NOT NULL,
    seq    INTEGER NOT NULL,
    UNIQUE (thread, seq)
);
CREATE TABLE blobs (
    hash  TEXT PRIMARY KEY,
    bytes BLOB NOT NULL
);
CREATE TABLE pending_prompts (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    thread    TEXT NOT NULL,
    prompt    TEXT NOT NULL,
    from_     TEXT NOT NULL,
    routed_by TEXT,
    source    TEXT NOT NULL,
    queued_at TEXT NOT NULL,
    said_after INTEGER,
    user_sent INTEGER
);
CREATE INDEX pending_prompts_thread ON pending_prompts (thread, id);
CREATE TABLE runner_acks (
    box    TEXT NOT NULL,
    thread TEXT NOT NULL,
    rseq   INTEGER NOT NULL,
    PRIMARY KEY (box, thread)
);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER control_no_update BEFORE UPDATE ON control
BEGIN SELECT RAISE(ABORT, 'control is append-only'); END;
CREATE TRIGGER control_no_delete BEFORE DELETE ON control
BEGIN SELECT RAISE(ABORT, 'control is append-only'); END;
"#;

/// Opens a connection with the hub's pragmas (WAL, busy timeout) without touching the schema.
pub fn connect(path: &Path) -> anyhow::Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

/// Creates or checks the schema.
fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!("database schema version {version} is newer than this twr ({SCHEMA_VERSION})");
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.transaction()?;
    if version == 0 {
        tx.execute_batch(SCHEMA)?;
    }
    // 1 → 2: `pending_prompts.said_after`. Rows queued before it stay NULL: their turns carry
    // no `said_after`, so the relay guard refuses to relay in them (fail-closed).
    if version == 1 {
        tx.execute_batch("ALTER TABLE pending_prompts ADD COLUMN said_after INTEGER;")?;
    }
    // → 3: `remote_outbox`.
    if version < 3 {
        tx.execute_batch(REMOTE_OUTBOX)?;
    }
    // → 4: `stop_requests`, `spawn_intents`, the kind index.
    tx.execute_batch(SCHEMA_4)?;
    // → 5: `attach_processes`.
    tx.execute_batch(SCHEMA_5)?;
    tx.execute_batch(SCHEMA_6)?;
    // → 7: no table; events a twr before it cannot decode (see the module docs).
    // → 8: `pending_prompts.user_sent`. Rows queued before it stay NULL: none of them
    // releases a crash's hold at the next start (the user sends again).
    if version != 0 {
        tx.execute_batch("ALTER TABLE pending_prompts ADD COLUMN user_sent INTEGER;")?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Kinds that end a runtime's life in the log: they retire its stop request and spawn intent.
const CLOSING_KINDS: [&str; 4] = [
    "thread.stopped",
    "thread.crashed",
    "thread.parked",
    "thread.attached",
];

/// Kinds that begin or end an attach: they retire the attach's recorded process.
const ATTACH_KINDS: [&str; 2] = ["thread.attached", "thread.detached"];

/// The interactive process of an attach (`attach_processes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachProcess {
    /// Its pid.
    pub pid: u32,
    /// Its start time ([`twr_runner::reap::process_start_time`]: on Linux clock ticks after
    /// boot, `/proc/<pid>/stat` field 22; on macOS seconds since the Unix epoch, from `ps`).
    pub start_time: u64,
    /// Its [`twr_runner::reap::ATTACH_ENV`] value.
    pub marker: String,
    /// The directory the attach works in on the thread's box.
    pub dir: Option<String>,
}

/// A prompt waiting to be dispatched as a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPrompt {
    /// Queue row id (dispatch order).
    pub id: i64,
    /// Thread id.
    pub thread: String,
    /// Prompt text.
    pub prompt: String,
    /// Who sent it.
    pub from: TurnFrom,
    /// Routing decision event id, if any.
    pub routed_by: Option<String>,
    /// Who queued it (becomes the `turn.started` source).
    pub source: Source,
    /// The control head when it was queued (`None` for rows from schema 1).
    pub said_after: Option<u64>,
}

/// A queued prompt as `thread.queue` lists it ([`Store::pending_list`]).
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedPrompt {
    /// Its row id: the message's ref, and its place in the queue.
    pub id: i64,
    /// The prompt.
    pub prompt: String,
    /// Who sent it.
    pub from: TurnFrom,
    /// Who queued it.
    pub source: Source,
    /// When (RFC 3339).
    pub queued_at: String,
    /// Marked as a send of the user's (`user_sent`).
    pub user_sent: bool,
}

/// The writer side of the database. The hub owns exactly one.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    /// Successful [`Store::sync`] calls (for tests: what was made durable when).
    syncs: std::sync::atomic::AtomicU64,
}

/// Extra writes committed together with an appended event.
#[derive(Debug, Default)]
pub struct AppendExtras<'a> {
    /// Blobs to store (`(ref, bytes)`), idempotent.
    pub blobs: &'a [(String, Vec<u8>)],
    /// Pending prompt row to remove (dispatch).
    pub dequeue: Option<i64>,
}

/// A prompt queued by [`Store::append_batch`] in the same transaction as the batch's events.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPrompt {
    /// Thread id.
    pub thread: String,
    /// Prompt text.
    pub prompt: String,
    /// Who sent it.
    pub from: TurnFrom,
    /// Routing decision event id, if any (may be an event of the same batch).
    pub routed_by: Option<String>,
    /// Who queued it (becomes the `turn.started` source).
    pub source: Source,
    /// The control head when it arrived; `None`: the head after the batch's own events (what
    /// [`Store::enqueue_prompt`] would get right after those events were appended one by one).
    pub said_after: Option<u64>,
    /// The user's own send (`pending_prompts.user_sent`, the head after the batch's events):
    /// what releases a queue a crash held.
    pub user_send: bool,
}

/// Several appends committed as one transaction ([`Store::append_batch`]): a logical operation
/// that must not be half-recorded (a crash or write failure between its parts would lose
/// work that no recovery can rebuild from the log).
#[derive(Debug, Default)]
pub struct AppendBatch<'a> {
    /// Events in append order, each with whether it gets a control row.
    pub events: Vec<(&'a Event, bool)>,
    /// Blobs to store (`(ref, bytes)`), idempotent.
    pub blobs: &'a [(String, Vec<u8>)],
    /// Pending prompt row to remove (dispatch).
    pub dequeue: Option<i64>,
    /// Prompts to queue, after the events.
    pub enqueue: Vec<NewPrompt>,
    /// A runner frame this batch commits: its watermark is raised in the same transaction.
    pub runner_ack: Option<&'a RunnerAck>,
    /// Commit a runner frame without syncing it to disk: the caller syncs a batch of such
    /// frames at once ([`Store::sync`]) before acknowledging any of them.
    pub defer_sync: bool,
    /// Commands for remote boxes this batch implies (`remote_outbox`).
    pub remote_cmds: &'a [RemoteCmd],
    /// Recorded commands (`(box, cmd_id)`) this batch retires (`remote_outbox`): an expired
    /// command goes with the event that ends its turn.
    pub remote_done: &'a [(String, String)],
    /// Threads whose runtime this batch asks to stop (`stop_requests`): a `thread.stop`
    /// (with [`AppendBatch::drop_queues`]), a settings restart, or a remote command that failed
    /// or expired and left the runtime in a state no later command can repair (`hub::runners`).
    pub stop_requests: &'a [String],
    /// Threads whose queued prompts this batch drops (before its events and queued prompts):
    /// a `thread.stop`.
    pub drop_queues: &'a [String],
}

/// A command for a remote box, recorded until the box answers it (`remote_outbox`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCmd {
    /// Box.
    pub box_name: String,
    /// Command id (the runner deduplicates by it).
    pub cmd_id: String,
    /// Thread it acts on.
    pub thread: String,
    /// The thread's open turn when it was issued (it no longer applies once another turn is
    /// open or none is).
    pub turn: Option<u64>,
    /// The operation (`twr_core::runner::Op` as JSON).
    pub op: String,
}

/// A committed runner frame: `rseq` of `thread` from box `box_name` (see `runner_acks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerAck {
    /// Box.
    pub box_name: String,
    /// Thread.
    pub thread: String,
    /// Runner sequence number.
    pub rseq: u64,
}

/// What [`Store::append_batch`] committed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchCommit {
    /// The control gseq of each event (`None` for non-control events), in batch order.
    pub gseqs: Vec<Option<u64>>,
    /// Queue row id of each queued prompt, in batch order.
    pub queued: Vec<i64>,
    /// Queued prompts dropped ([`AppendBatch::drop_queues`]).
    pub dropped: usize,
}

impl Store {
    /// Opens (creating if needed) the database at `path`.
    ///
    /// The log holds every prompt, transcript and answer, so it is private to its owner
    /// whatever the umask: the directory is made 0700 ([`twr_core::paths::ensure_private_dir`])
    /// and the database is created 0600 before SQLite opens it (SQLite gives the `-wal` and
    /// `-shm` files it creates the database file's mode). Existing files others can read are
    /// restricted.
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            twr_core::paths::ensure_private_dir(dir)?;
        }
        create_private(path)?;
        let mut conn = connect(path)?;
        migrate(&mut conn)?;
        restrict_db_files(path)?;
        Ok(Store {
            conn,
            syncs: Default::default(),
        })
    }

    /// The underlying connection (reads).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Appends `ev` (and its control row when `control`), plus `extras`, in one transaction.
    /// Returns the control stream gseq, if any.
    pub fn append(
        &mut self,
        ev: &Event,
        control: bool,
        extras: AppendExtras<'_>,
    ) -> anyhow::Result<Option<u64>> {
        let commit = self.append_batch(AppendBatch {
            events: vec![(ev, control)],
            blobs: extras.blobs,
            dequeue: extras.dequeue,
            enqueue: Vec::new(),
            runner_ack: None,
            defer_sync: false,
            remote_cmds: &[],
            remote_done: &[],
            stop_requests: &[],
            drop_queues: &[],
        })?;
        Ok(commit.gseqs.into_iter().next().flatten())
    }

    /// Commits `batch` in one transaction: its blobs, the dequeued prompt, every event (with
    /// its control row) in order, then the queued prompts. Either all of it is recorded or
    /// none of it.
    pub fn append_batch(&mut self, batch: AppendBatch<'_>) -> anyhow::Result<BatchCommit> {
        let data = batch
            .events
            .iter()
            .map(|(ev, _)| Ok(serde_json::to_string(&ev.body.data()?)?))
            .collect::<anyhow::Result<Vec<String>>>()?;
        // A runner frame is acked once this commits, and the runner then deletes it: that
        // commit must survive a power loss too, not only a crash of the hub (unless the caller
        // syncs a batch of frames before acking them, `defer_sync`).
        let durable = batch.runner_ack.is_some() && !batch.defer_sync;
        if durable {
            self.conn.pragma_update(None, "synchronous", "FULL")?;
        }
        let r = Self::commit_batch(&mut self.conn, batch, data);
        if durable {
            if let Err(e) = self.conn.pragma_update(None, "synchronous", "NORMAL") {
                tracing::warn!(error = %e, "cannot restore synchronous = NORMAL");
            }
        }
        r
    }

    fn commit_batch(
        conn: &mut Connection,
        batch: AppendBatch<'_>,
        data: Vec<String>,
    ) -> anyhow::Result<BatchCommit> {
        let tx = conn.transaction()?;
        for (r, bytes) in batch.blobs {
            tx.execute(
                "INSERT OR IGNORE INTO blobs (hash, bytes) VALUES (?1, ?2)",
                params![r, bytes],
            )?;
        }
        if let Some(id) = batch.dequeue {
            let n = tx.execute("DELETE FROM pending_prompts WHERE id = ?1", params![id])?;
            if n != 1 {
                bail!("pending prompt {id} is gone");
            }
        }
        let mut out = BatchCommit::default();
        for thread in batch.drop_queues {
            out.dropped += tx.execute(
                "DELETE FROM pending_prompts WHERE thread = ?1",
                params![thread],
            )?;
        }
        for ((ev, control), data) in batch.events.iter().zip(data) {
            tx.execute(
                "INSERT INTO events (thread, seq, id, v, ts, turn, source, kind, data, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    ev.thread,
                    to_i64(ev.seq)?,
                    ev.id,
                    ev.v,
                    ev.ts.to_rfc3339_opts(SecondsFormat::Millis, true),
                    ev.turn.map(to_i64).transpose()?,
                    ev.source.to_string(),
                    ev.kind(),
                    data,
                    ev.raw,
                ],
            )?;
            let gseq = if *control {
                tx.execute(
                    "INSERT INTO control (thread, seq) VALUES (?1, ?2)",
                    params![ev.thread, to_i64(ev.seq)?],
                )?;
                Some(tx.last_insert_rowid() as u64)
            } else {
                None
            };
            out.gseqs.push(gseq);
            if CLOSING_KINDS.contains(&ev.kind()) {
                tx.execute(
                    "DELETE FROM stop_requests WHERE thread = ?1",
                    params![ev.thread],
                )?;
                tx.execute(
                    "DELETE FROM spawn_intents WHERE thread = ?1",
                    params![ev.thread],
                )?;
            }
            if ATTACH_KINDS.contains(&ev.kind()) {
                tx.execute(
                    "DELETE FROM attach_processes WHERE thread = ?1",
                    params![ev.thread],
                )?;
            }
        }
        for p in &batch.enqueue {
            let head = control_head(&tx)?;
            out.queued
                .push(insert_prompt(&tx, p, p.said_after.unwrap_or(head), head)?);
        }
        if let Some(a) = batch.runner_ack {
            tx.execute(
                "INSERT INTO runner_acks (box, thread, rseq) VALUES (?1, ?2, ?3)
                 ON CONFLICT(box, thread) DO UPDATE SET rseq = MAX(rseq, excluded.rseq)",
                params![a.box_name, a.thread, to_i64(a.rseq)?],
            )?;
        }
        if !batch.remote_cmds.is_empty() {
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            for c in batch.remote_cmds {
                tx.execute(
                    "INSERT OR IGNORE INTO remote_outbox (box, cmd_id, thread, turn, op, created)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        c.box_name,
                        c.cmd_id,
                        c.thread,
                        c.turn.map(to_i64).transpose()?,
                        c.op,
                        now
                    ],
                )?;
            }
        }
        for (box_name, cmd_id) in batch.remote_done {
            tx.execute(
                "DELETE FROM remote_outbox WHERE box = ?1 AND cmd_id = ?2",
                params![box_name, cmd_id],
            )?;
        }
        for thread in batch.stop_requests {
            tx.execute(
                "INSERT OR REPLACE INTO stop_requests (thread, ts) VALUES (?1, ?2)",
                params![
                    thread,
                    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
                ],
            )?;
        }
        tx.commit()?;
        Ok(out)
    }

    /// Makes every transaction committed so far survive a power loss, as `synchronous = FULL`
    /// does for each commit: syncs the write-ahead log, where commits made with `synchronous =
    /// NORMAL` wait until a checkpoint (which syncs what it moves into the database itself).
    /// For runner frames committed with [`AppendBatch::defer_sync`]: one sync covers them all,
    /// and only then are they acknowledged. SQLite writes a commit to the log file before the
    /// commit returns; syncing the file through any descriptor makes those writes durable.
    pub fn sync(&self) -> anyhow::Result<()> {
        let Some(db) = self.conn.path().filter(|p| !p.is_empty()) else {
            return Ok(());
        };
        let wal = format!("{db}-wal");
        match std::fs::File::open(&wal) {
            Ok(f) => f.sync_all().with_context(|| format!("syncing {wal}"))?,
            // No log: everything is in the database file, synced by the checkpoint that
            // emptied it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::Error::new(e).context(format!("opening {wal}"))),
        }
        self.syncs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// How many [`Store::sync`] calls succeeded so far.
    pub fn sync_count(&self) -> u64 {
        self.syncs.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The committed watermark of `thread` from box `box_name` (0: nothing yet).
    pub fn runner_ack(&self, box_name: &str, thread: &str) -> anyhow::Result<u64> {
        let n: Option<i64> = self
            .conn
            .query_row(
                "SELECT rseq FROM runner_acks WHERE box = ?1 AND thread = ?2",
                params![box_name, thread],
                |r| r.get(0),
            )
            .optional()?;
        Ok(n.map_or(0, |n| n as u64))
    }

    /// The commands box `box_name` still has to get (`remote_outbox`), in the order they were
    /// issued.
    pub fn remote_cmds(&self, box_name: &str) -> anyhow::Result<Vec<RemoteCmd>> {
        let mut st = self.conn.prepare(
            "SELECT box, cmd_id, thread, turn, op FROM remote_outbox WHERE box = ?1 ORDER BY id",
        )?;
        let rows = st.query_map([box_name], |r| {
            Ok(RemoteCmd {
                box_name: r.get(0)?,
                cmd_id: r.get(1)?,
                thread: r.get(2)?,
                turn: r.get::<_, Option<i64>>(3)?.map(|t| t as u64),
                op: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forgets command `cmd_id` of box `box_name` (answered, or no longer applies). Returns
    /// whether it was recorded.
    pub fn remote_cmd_done(&self, box_name: &str, cmd_id: &str) -> anyhow::Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM remote_outbox WHERE box = ?1 AND cmd_id = ?2",
            params![box_name, cmd_id],
        )?;
        Ok(n > 0)
    }

    /// Whether `thread` has a recorded `respond` (`remote_outbox`) its box has not answered.
    pub fn unanswered_respond(&self, thread: &str) -> anyhow::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM remote_outbox
             WHERE thread = ?1 AND substr(cmd_id, 1, 8) = 'respond:')",
            params![thread],
            |r| r.get(0),
        )?)
    }

    /// Command `cmd_id` of box `box_name`, while it is recorded.
    pub fn remote_cmd(&self, box_name: &str, cmd_id: &str) -> anyhow::Result<Option<RemoteCmd>> {
        Ok(self
            .conn
            .query_row(
                "SELECT box, cmd_id, thread, turn, op FROM remote_outbox
                 WHERE box = ?1 AND cmd_id = ?2",
                params![box_name, cmd_id],
                |r| {
                    Ok(RemoteCmd {
                        box_name: r.get(0)?,
                        cmd_id: r.get(1)?,
                        thread: r.get(2)?,
                        turn: r.get::<_, Option<i64>>(3)?.map(|t| t as u64),
                        op: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Whether `thread`'s runtime recorded anything in turn `turn` (it got the turn's prompt).
    pub fn turn_has_runtime_events(&self, thread: &str, turn: u64) -> anyhow::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM events
             WHERE thread = ?1 AND turn = ?2 AND source LIKE 'adapter:%')",
            params![thread, to_i64(turn)?],
            |r| r.get(0),
        )?)
    }

    /// The commands of box `box_name` recorded before `cutoff` (see [`OUTBOX_RETENTION`]), in
    /// the order they were issued. Read only: each is retired with what its expiry records
    /// ([`AppendBatch::remote_done`], or [`Store::remote_cmd_done`] when that is nothing).
    pub fn expired_remote_cmds(
        &self,
        box_name: &str,
        cutoff: DateTime<Utc>,
    ) -> anyhow::Result<Vec<RemoteCmd>> {
        let cutoff = cutoff.to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut st = self.conn.prepare(
            "SELECT box, cmd_id, thread, turn, op FROM remote_outbox
             WHERE box = ?1 AND created < ?2 ORDER BY id",
        )?;
        let rows = st.query_map(params![box_name, cutoff], |r| {
            Ok(RemoteCmd {
                box_name: r.get(0)?,
                cmd_id: r.get(1)?,
                thread: r.get(2)?,
                turn: r.get::<_, Option<i64>>(3)?.map(|t| t as u64),
                op: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Threads with a stop request whose closing sequence is not recorded yet.
    pub fn stop_requests(&self) -> anyhow::Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT thread FROM stop_requests ORDER BY thread")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records that a local runtime of `thread` is being started by a hub whose own process
    /// started at `min_start` (see `spawn_intents`).
    pub fn add_spawn_intent(&mut self, thread: &str, min_start: u64) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO spawn_intents (thread, min_start, ts) VALUES (?1, ?2, ?3)",
            params![
                thread,
                to_i64(min_start)?,
                Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
            ],
        )?;
        Ok(())
    }

    /// Forgets `thread`'s spawn intent.
    pub fn clear_spawn_intent(&mut self, thread: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "DELETE FROM spawn_intents WHERE thread = ?1",
            params![thread],
        )?;
        Ok(())
    }

    /// Every spawn intent: `(thread, min_start)`.
    pub fn spawn_intents(&self) -> anyhow::Result<Vec<(String, u64)>> {
        let mut st = self
            .conn
            .prepare("SELECT thread, min_start FROM spawn_intents ORDER BY thread")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records the interactive process of `thread`'s attach (see `attach_processes`).
    pub fn set_attach_process(&mut self, thread: &str, p: &AttachProcess) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO attach_processes (thread, pid, start_time, marker, dir, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                thread,
                i64::from(p.pid),
                to_i64(p.start_time)?,
                p.marker,
                p.dir,
                Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
            ],
        )?;
        Ok(())
    }

    /// The recorded interactive process of `thread`'s attach, if any.
    pub fn attach_process(&self, thread: &str) -> anyhow::Result<Option<AttachProcess>> {
        Ok(self
            .conn
            .query_row(
                "SELECT pid, start_time, marker, dir FROM attach_processes WHERE thread = ?1",
                params![thread],
                |r| {
                    Ok(AttachProcess {
                        pid: u32::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                        start_time: r.get::<_, i64>(1)?.max(0) as u64,
                        marker: r.get(2)?,
                        dir: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Every committed watermark of box `box_name`, by thread.
    pub fn runner_watermarks(
        &self,
        box_name: &str,
    ) -> anyhow::Result<std::collections::BTreeMap<String, u64>> {
        let mut st = self
            .conn
            .prepare("SELECT thread, rseq FROM runner_acks WHERE box = ?1")?;
        let rows = st.query_map([box_name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every event in append order.
    pub fn load_all(&self) -> anyhow::Result<Vec<Event>> {
        let mut st = self
            .conn
            .prepare(&format!("SELECT {EVENT_COLS} FROM events ORDER BY rowid"))?;
        let rows = st.query_map([], row_to_raw)?;
        rows.map(|r| r?.into_event()).collect()
    }

    /// Every event in append order with its control gseq (`None` for non-control events).
    pub fn load_all_with_gseq(&self) -> anyhow::Result<Vec<(Event, Option<u64>)>> {
        let cols = EVENT_COLS
            .split(", ")
            .map(|c| format!("e.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut st = self.conn.prepare(&format!(
            "SELECT {cols}, c.gseq FROM events e
             LEFT JOIN control c ON c.thread = e.thread AND c.seq = e.seq
             ORDER BY e.rowid"
        ))?;
        let rows = st.query_map([], |r| {
            let gseq: Option<i64> = r.get(10)?;
            Ok((row_to_raw(r)?, gseq))
        })?;
        rows.map(|r| {
            let (raw, gseq) = r?;
            Ok((raw.into_event()?, gseq.map(|g| g as u64)))
        })
        .collect()
    }

    /// Highest control gseq (0 when empty).
    pub fn control_head(&self) -> anyhow::Result<u64> {
        control_head(&self.conn)
    }

    /// Queues a prompt that arrived when the control head was `said_after`; returns its row
    /// id.
    pub fn enqueue_prompt(
        &mut self,
        thread: &str,
        prompt: &str,
        from: TurnFrom,
        routed_by: Option<&str>,
        source: &Source,
        said_after: u64,
    ) -> anyhow::Result<i64> {
        let p = NewPrompt {
            thread: thread.to_string(),
            prompt: prompt.to_string(),
            from,
            routed_by: routed_by.map(str::to_string),
            source: source.clone(),
            said_after: Some(said_after),
            user_send: false,
        };
        let head = control_head(&self.conn)?;
        insert_prompt(&self.conn, &p, said_after, head)
    }

    /// Whether `thread` has a prompt the user sent queued at control head `gseq` or later
    /// (`pending_prompts.user_sent`: a job's wake, the orchestrator's send and rows from
    /// before schema 8 never count). Not `said_after`: `say` caps that at what the user had
    /// seen, which can be before the crash the send came after.
    pub fn user_sent_since(&self, thread: &str, gseq: u64) -> anyhow::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pending_prompts WHERE thread = ?1 AND user_sent >= ?2)",
            params![thread, to_i64(gseq)?],
            |r| r.get(0),
        )?)
    }

    /// Whether queued prompt `id` is the user's own send ([`NewPrompt::user_send`]); false
    /// once it is no longer queued.
    pub fn is_user_send(&self, id: i64) -> anyhow::Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT user_sent IS NOT NULL FROM pending_prompts WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// The oldest queued prompt of `thread`.
    pub fn next_pending(&self, thread: &str) -> anyhow::Result<Option<PendingPrompt>> {
        self.conn
            .query_row(
                "SELECT id, thread, prompt, from_, routed_by, source, said_after FROM pending_prompts
                 WHERE thread = ?1 ORDER BY id LIMIT 1",
                params![thread],
                row_to_pending,
            )
            .optional()?
            .map(pending_from_row)
            .transpose()
    }

    /// Queued prompt `id` (a message's ref), while it is still queued.
    pub fn pending_prompt(&self, id: i64) -> anyhow::Result<Option<PendingPrompt>> {
        self.conn
            .query_row(
                "SELECT id, thread, prompt, from_, routed_by, source, said_after FROM pending_prompts
                 WHERE id = ?1",
                params![id],
                row_to_pending,
            )
            .optional()?
            .map(pending_from_row)
            .transpose()
    }

    /// The first `limit` queued prompts of `thread`, in the order they run (`thread.queue`).
    pub fn pending_list(&self, thread: &str, limit: usize) -> anyhow::Result<Vec<QueuedPrompt>> {
        let mut st = self.conn.prepare(
            "SELECT id, prompt, from_, source, queued_at, user_sent IS NOT NULL
             FROM pending_prompts WHERE thread = ?1 ORDER BY id LIMIT ?2",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = st.query_map(params![thread, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, bool>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, prompt, from, source, queued_at, user_sent) = row?;
            out.push(QueuedPrompt {
                id,
                prompt,
                from: turn_from_parse(&from)?,
                source: Source::from_str(&source)?,
                queued_at,
                user_sent,
            });
        }
        Ok(out)
    }

    /// The newest queued prompt of `thread` that a user sent (a client of theirs, the row
    /// marked as their send: a gated send let go later holds reports agents wrote), wherever
    /// it is in the queue: what the TUI's `↑` takes back to edit.
    pub fn newest_user_pending(&self, thread: &str) -> anyhow::Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM pending_prompts WHERE thread = ?1 AND from_ = 'user'
                 AND source LIKE 'user:%' AND user_sent IS NOT NULL ORDER BY id DESC LIMIT 1",
                params![thread],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Records that the user asked for queued prompt `id` to run (`message.send_now` on a held
    /// queue), at control head `head`, as a send of theirs records it (`user_sent`): a hub
    /// started after a crash finds the hold released ([`Store::user_sent_since`]). The row's
    /// sender and authority (`from_`, `source`, `said_after`) stay as they were. Whether a row
    /// changed.
    pub fn mark_user_sent(&mut self, id: i64, head: u64) -> anyhow::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE pending_prompts SET user_sent = ?2 WHERE id = ?1 AND user_sent IS NULL",
            params![id, to_i64(head)?],
        )? > 0)
    }

    /// Number of queued prompts of `thread`.
    pub fn pending_count(&self, thread: &str) -> anyhow::Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pending_prompts WHERE thread = ?1",
            params![thread],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Threads that have queued prompts.
    pub fn threads_with_pending(&self) -> anyhow::Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT DISTINCT thread FROM pending_prompts ORDER BY thread")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Drops every queued prompt of `thread`; returns how many.
    pub fn clear_pending(&mut self, thread: &str) -> anyhow::Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM pending_prompts WHERE thread = ?1",
            params![thread],
        )?)
    }
}

/// Inserts a `pending_prompts` row queued at control head `head`; returns its id.
fn insert_prompt(
    conn: &Connection,
    p: &NewPrompt,
    said_after: u64,
    head: u64,
) -> anyhow::Result<i64> {
    let user_sent = if p.user_send {
        Some(to_i64(head)?)
    } else {
        None
    };
    conn.execute(
        "INSERT INTO pending_prompts
             (thread, prompt, from_, routed_by, source, queued_at, said_after, user_sent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            p.thread,
            p.prompt,
            turn_from_str(p.from),
            p.routed_by,
            p.source.to_string(),
            Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            to_i64(said_after)?,
            user_sent,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Creates `path` empty with mode 0600 if it does not exist (SQLite then initializes it).
fn create_private(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow!("creating {}: {e}", path.display())),
    }
}

/// Restricts the database and its `-wal` / `-shm` files to their owner (mode 0600).
fn restrict_db_files(path: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut f = path.as_os_str().to_owned();
        f.push(suffix);
        let f = std::path::PathBuf::from(f);
        twr_core::paths::restrict_file(&f)
            .with_context(|| format!("restricting {}", f.display()))?;
    }
    Ok(())
}

fn to_i64(n: u64) -> anyhow::Result<i64> {
    i64::try_from(n).map_err(|_| anyhow!("{n} does not fit in SQLite INTEGER"))
}

fn turn_from_str(f: TurnFrom) -> &'static str {
    match f {
        TurnFrom::User => "user",
        TurnFrom::Orchestrator => "orchestrator",
        TurnFrom::Runtime => "runtime",
    }
}

type PendingRow = (
    i64,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<i64>,
);

fn row_to_pending(r: &Row<'_>) -> rusqlite::Result<PendingRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

/// A `pending_prompts.from_` value.
fn turn_from_parse(from: &str) -> anyhow::Result<TurnFrom> {
    Ok(match from {
        "user" => TurnFrom::User,
        "orchestrator" => TurnFrom::Orchestrator,
        "runtime" => TurnFrom::Runtime,
        other => bail!("bad pending prompt sender {other:?}"),
    })
}

fn pending_from_row(
    (id, thread, prompt, from, routed_by, source, said_after): PendingRow,
) -> anyhow::Result<PendingPrompt> {
    let from = turn_from_parse(&from)?;
    Ok(PendingPrompt {
        id,
        thread,
        prompt,
        from,
        routed_by,
        source: Source::from_str(&source)?,
        said_after: said_after.and_then(|n| u64::try_from(n).ok()),
    })
}

/// The columns of an event row, in [`row_to_raw`]'s order (a macro, for `concat!`).
macro_rules! event_cols {
    () => {
        "thread, seq, id, v, ts, turn, source, kind, data, raw"
    };
}

const EVENT_COLS: &str = event_cols!();

/// An event row before parsing.
struct RawRow {
    thread: String,
    seq: i64,
    id: String,
    v: u32,
    ts: String,
    turn: Option<i64>,
    source: String,
    kind: String,
    data: String,
    raw: Option<String>,
}

fn row_to_raw(r: &Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        thread: r.get(0)?,
        seq: r.get(1)?,
        id: r.get(2)?,
        v: r.get(3)?,
        ts: r.get(4)?,
        turn: r.get(5)?,
        source: r.get(6)?,
        kind: r.get(7)?,
        data: r.get(8)?,
        raw: r.get(9)?,
    })
}

impl RawRow {
    fn into_event(self) -> anyhow::Result<Event> {
        let body = EventBody::from_kind_json(&self.kind, Some(&self.data))
            .with_context(|| format!("event {} ({}) data", self.id, self.kind))?;
        Ok(Event {
            v: self.v,
            seq: self.seq as u64,
            ts: DateTime::parse_from_rfc3339(&self.ts)?.with_timezone(&Utc),
            thread: self.thread,
            turn: self.turn.map(|t| t as u64),
            source: Source::from_str(&self.source)?,
            body,
            raw: self.raw,
            id: self.id,
        })
    }
}

/// Events of `thread` with `seq > since_seq`, oldest first, at most `limit`.
pub fn read_thread_events(
    conn: &Connection,
    thread: &str,
    since_seq: u64,
    limit: usize,
) -> anyhow::Result<Vec<Event>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {EVENT_COLS} FROM events WHERE thread = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
    ))?;
    let rows = st.query_map(
        params![thread, to_i64(since_seq)?, limit as i64],
        row_to_raw,
    )?;
    rows.map(|r| r?.into_event()).collect()
}

/// [`read_thread_events`] as the lines a thread subscription writes ([`stream_line`]), each
/// with its seq.
pub fn read_thread_lines(
    conn: &Connection,
    thread: &str,
    since_seq: u64,
    limit: usize,
) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {EVENT_COLS} FROM events WHERE thread = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
    ))?;
    let rows = st.query_map(
        params![thread, to_i64(since_seq)?, limit as i64],
        row_to_raw,
    )?;
    rows.map(|r| {
        let row = r?;
        let seq = row.seq as u64;
        let mut line = Vec::new();
        stream_line(row, None, &mut line)?;
        Ok((seq, line))
    })
    .collect()
}

/// The line a subscription writes for event row `row` (with its newline), a control item
/// when it has a `gseq`: what serializing its [`twr_core::proto::StreamItem`] writes, read
/// back the same. The row's `data` goes in as stored, without parsing it into its type and
/// writing that back out, which costs a replay of thousands most of its time. Only a row
/// whose `data` is not one JSON value on one line, or whose `ts` or `source` does not read,
/// goes through its type ([`RawRow::into_event`]), which then also fails as it would.
fn stream_line(row: RawRow, gseq: Option<u64>, out: &mut Vec<u8>) -> anyhow::Result<()> {
    use serde_json::value::RawValue;
    let data = serde_json::from_str::<&RawValue>(&row.data)
        .ok()
        .filter(|d| !d.get().contains(['\n', '\r']));
    // The envelope as `Event` writes it: the time to the millisecond in UTC, the source
    // spelled its way.
    let ts = DateTime::parse_from_rfc3339(&row.ts).ok().map(|t| {
        t.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    });
    let source = Source::from_str(&row.source).ok().map(|s| s.to_string());
    let readable = row.seq >= 0 && row.turn.is_none_or(|t| t >= 0);
    let (Some(data), Some(ts), Some(source), true) = (data, ts, source, readable) else {
        let item = twr_core::proto::StreamItem {
            stream: if gseq.is_some() {
                twr_core::proto::StreamKind::Control
            } else {
                twr_core::proto::StreamKind::Thread
            },
            gseq,
            event: row.into_event()?,
        };
        serde_json::to_writer(&mut *out, &item)?;
        out.push(b'\n');
        return Ok(());
    };
    let text = |out: &mut Vec<u8>, s: &str| serde_json::to_writer(&mut *out, s);
    match gseq {
        Some(g) => {
            out.extend_from_slice(br#"{"stream":"control","gseq":"#);
            out.extend_from_slice(g.to_string().as_bytes());
        }
        None => out.extend_from_slice(br#"{"stream":"thread""#),
    }
    out.extend_from_slice(br#","event":{"v":"#);
    out.extend_from_slice(row.v.to_string().as_bytes());
    out.extend_from_slice(br#","id":"#);
    text(out, &row.id)?;
    out.extend_from_slice(br#","seq":"#);
    out.extend_from_slice(row.seq.to_string().as_bytes());
    out.extend_from_slice(br#","ts":"#);
    text(out, &ts)?;
    out.extend_from_slice(br#","thread":"#);
    text(out, &row.thread)?;
    out.extend_from_slice(br#","turn":"#);
    match row.turn {
        Some(t) => out.extend_from_slice(t.to_string().as_bytes()),
        None => out.extend_from_slice(b"null"),
    }
    out.extend_from_slice(br#","source":"#);
    text(out, &source)?;
    out.extend_from_slice(br#","kind":"#);
    text(out, &row.kind)?;
    out.extend_from_slice(br#","data":"#);
    out.extend_from_slice(data.get().as_bytes());
    out.extend_from_slice(br#","raw":"#);
    match &row.raw {
        Some(r) => text(out, r)?,
        None => out.extend_from_slice(b"null"),
    }
    out.extend_from_slice(b"}}\n");
    Ok(())
}

/// Semantic transcript rows. Hidden streaming text never consumes a page slot.
pub fn read_transcript_events(
    conn: &Connection,
    thread: &str,
    since: u64,
    before: u64,
    limit: usize,
) -> anyhow::Result<Vec<Event>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {EVENT_COLS} FROM events WHERE thread = ?1 AND seq > ?2 AND seq < ?3
         AND (kind IN ('turn.started', 'turn.ended', 'agent.tool_call', 'agent.tool_result',
              'attention.raised', 'attention.resolved', 'attention.annotated', 'report.ready',
              'thread.stopped', 'thread.crashed', 'thread.mode_changed', 'thread.model_changed',
              'thread.renamed', 'conversation.update', 'thread.deleted', 'thread.restored',
              'project.read')
              OR (kind = 'task.updated' AND COALESCE(json_extract(data, '$.heartbeat'), 0) = 0)
              OR (kind = 'agent.text' AND json_extract(data, '$.final') = 1))
         ORDER BY seq LIMIT ?4"
    ))?;
    let rows = st.query_map(
        params![thread, to_i64(since)?, to_i64(before)?, limit as i64],
        row_to_raw,
    )?;
    rows.map(|r| r?.into_event()).collect()
}

/// The sequence bounds of a turn, including lifecycle events after its end.
pub fn transcript_bounds(
    conn: &Connection,
    thread: &str,
    turn: Option<u64>,
) -> anyhow::Result<(u64, u64, Option<u64>)> {
    let start: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM events WHERE thread = ?1 AND kind = 'turn.started'
         AND (?2 IS NULL OR turn = ?2)",
        params![thread, turn.map(to_i64).transpose()?],
        |r| r.get(0),
    )?;
    if turn.is_some() && start.is_none() {
        bail!("turn not found");
    }
    let chosen = start
        .map(|seq| {
            conn.query_row(
                "SELECT turn FROM events WHERE thread=?1 AND seq=?2",
                params![thread, seq],
                |r| r.get::<_, Option<i64>>(0),
            )
        })
        .transpose()?
        .flatten()
        .map(|t| t as u64);
    let start = start.unwrap_or(1);
    let next: Option<i64> = conn.query_row(
        "SELECT MIN(seq) FROM events WHERE thread = ?1 AND kind = 'turn.started' AND seq > ?2",
        params![thread, start],
        |r| r.get(0),
    )?;
    Ok((
        start.saturating_sub(1) as u64,
        next.unwrap_or(i64::MAX) as u64,
        chosen,
    ))
}

/// Control items with `gseq > since`, oldest first, at most `limit`.
pub fn read_control(
    conn: &Connection,
    since: u64,
    limit: usize,
) -> anyhow::Result<Vec<(u64, Event)>> {
    let cols = EVENT_COLS
        .split(", ")
        .map(|c| format!("e.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut st = conn.prepare_cached(&format!(
        "SELECT c.gseq, {cols} FROM control c JOIN events e ON e.thread = c.thread AND e.seq = c.seq
         WHERE c.gseq > ?1 ORDER BY c.gseq LIMIT ?2"
    ))?;
    let rows = st.query_map(params![to_i64(since)?, limit as i64], |r| {
        let gseq: i64 = r.get(0)?;
        Ok((
            gseq as u64,
            RawRow {
                thread: r.get(1)?,
                seq: r.get(2)?,
                id: r.get(3)?,
                v: r.get(4)?,
                ts: r.get(5)?,
                turn: r.get(6)?,
                source: r.get(7)?,
                kind: r.get(8)?,
                data: r.get(9)?,
                raw: r.get(10)?,
            },
        ))
    })?;
    rows.map(|r| {
        let (g, raw) = r?;
        Ok((g, raw.into_event()?))
    })
    .collect()
}

/// [`read_control`] as the lines a control subscription writes ([`stream_line`]).
pub fn read_control_lines(
    conn: &Connection,
    since: u64,
    limit: usize,
) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
    let cols = EVENT_COLS
        .split(", ")
        .map(|c| format!("e.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut st = conn.prepare_cached(&format!(
        "SELECT c.gseq, {cols} FROM control c JOIN events e ON e.thread = c.thread AND e.seq = c.seq
         WHERE c.gseq > ?1 ORDER BY c.gseq LIMIT ?2"
    ))?;
    let rows = st.query_map(params![to_i64(since)?, limit as i64], |r| {
        let gseq: i64 = r.get(0)?;
        Ok((
            gseq as u64,
            RawRow {
                thread: r.get(1)?,
                seq: r.get(2)?,
                id: r.get(3)?,
                v: r.get(4)?,
                ts: r.get(5)?,
                turn: r.get(6)?,
                source: r.get(7)?,
                kind: r.get(8)?,
                data: r.get(9)?,
                raw: r.get(10)?,
            },
        ))
    })?;
    rows.map(|r| {
        let (g, row) = r?;
        let mut line = Vec::new();
        stream_line(row, Some(g), &mut line)?;
        Ok((g, line))
    })
    .collect()
}

/// Highest control gseq (0 when empty).
pub fn control_head(conn: &Connection) -> anyhow::Result<u64> {
    let n: Option<i64> = conn.query_row("SELECT MAX(gseq) FROM control", [], |r| r.get(0))?;
    Ok(n.unwrap_or(0) as u64)
}

/// Highest seq of `thread` (0 when empty).
pub fn thread_head(conn: &Connection, thread: &str) -> anyhow::Result<u64> {
    let n: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM events WHERE thread = ?1",
        params![thread],
        |r| r.get(0),
    )?;
    Ok(n.unwrap_or(0) as u64)
}

/// Fetches a blob.
pub fn get_blob(conn: &Connection, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
    Ok(conn
        .query_row(
            "SELECT bytes FROM blobs WHERE hash = ?1",
            params![blob_ref],
            |r| r.get(0),
        )
        .optional()?)
}

/// Up to `len` bytes of a blob starting at byte `offset`, plus the blob's total size. `None`
/// when the blob does not exist. Reads only the requested range.
pub fn get_blob_chunk(
    conn: &Connection,
    blob_ref: &str,
    offset: u64,
    len: usize,
) -> anyhow::Result<Option<(u64, Vec<u8>)>> {
    let row: Option<(i64, Option<Vec<u8>>)> = conn
        .query_row(
            // substr() on a BLOB counts bytes, 1-based.
            "SELECT length(bytes), substr(bytes, ?2, ?3) FROM blobs WHERE hash = ?1",
            params![blob_ref, to_i64(offset)?.saturating_add(1), len as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(size, bytes)| (size as u64, bytes.unwrap_or_default())))
}

/// What the hub already appended in reaction to turn `turn` of `thread` ending (grade decision,
/// report, review attention), so the reaction can be completed idempotently after a crash.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnReactions {
    /// A `decision.made{site: grade}` exists.
    pub graded: bool,
    /// `chosen` of the latest `decision.made{site: grade}`, when it is a grade.
    pub grade: Option<Grade>,
    /// A `report.ready` exists.
    pub reported: bool,
    /// The grade of the latest `report.ready`.
    pub report_grade: Option<Grade>,
    /// A hub-raised `attention.raised{reason: review}` exists.
    pub reviewed: bool,
    /// `chosen` of the latest `decision.made{site: block_kind}` (the ladder's accepted answer,
    /// or the default the hub logged after it), if the failure was classified.
    pub block_kind: Option<String>,
    /// A hub-raised `attention.raised{reason: error}` exists for this turn.
    pub errored: bool,
}

/// Reads the [`TurnReactions`] of a turn from the log.
pub fn turn_reactions(conn: &Connection, thread: &str, turn: u64) -> anyhow::Result<TurnReactions> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {EVENT_COLS} FROM events WHERE thread = ?1 AND turn = ?2
         AND kind IN ('decision.made', 'report.ready', 'attention.raised') ORDER BY seq"
    ))?;
    let rows = st.query_map(params![thread, to_i64(turn)?], row_to_raw)?;
    let mut out = TurnReactions::default();
    for r in rows {
        let ev = r?.into_event()?;
        match &ev.body {
            EventBody::DecisionMade(d) if d.site == DecisionSite::Grade => {
                out.graded = true;
                out.grade = twr_decisions::parse_grade(&d.chosen);
            }
            EventBody::DecisionMade(d) if d.site == DecisionSite::BlockKind => {
                out.block_kind = Some(d.chosen.clone())
            }
            EventBody::ReportReady(r) => {
                out.reported = true;
                out.report_grade = Some(r.grade);
            }
            EventBody::AttentionRaised(a)
                if a.reason == AttentionReason::Review && ev.source == Source::Hub =>
            {
                out.reviewed = true
            }
            EventBody::AttentionRaised(a)
                if a.reason == AttentionReason::Error && ev.source == Source::Hub =>
            {
                out.errored = true
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Every `turn.ended` (as stored) of a turn that has no `report.ready`, oldest first, except
/// on the threads in `skip` (the orchestrators: their turns get no report): the turns whose
/// reactions startup recovery completes (`Core::recover_turn_reactions`).
pub fn unreported_turn_ends(conn: &Connection, skip: &[String]) -> anyhow::Result<Vec<Event>> {
    let mut st = conn.prepare(&format!(
        "SELECT {EVENT_COLS} FROM events e WHERE e.kind = 'turn.ended' AND e.turn IS NOT NULL
         AND NOT EXISTS (SELECT 1 FROM events r WHERE r.kind = 'report.ready'
                         AND r.thread = e.thread AND r.turn = e.turn)
         AND e.thread NOT IN (SELECT value FROM json_each(?1))
         ORDER BY e.rowid"
    ))?;
    let rows = st.query_map(params![serde_json::to_string(skip)?], row_to_raw)?;
    rows.map(|r| r?.into_event()).collect()
}

/// The turns of `thread` that ended `error` (as recorded) and have no hub-raised
/// `attention.raised{reason: error}`, oldest first: the failed orchestrator turns whose item
/// startup recovery raises (`Core::recover_turn_reactions`).
pub fn failed_turns_without_error_item(
    conn: &Connection,
    thread: &str,
) -> anyhow::Result<Vec<u64>> {
    let mut st = conn.prepare_cached(
        "SELECT e.turn FROM events e WHERE e.kind = 'turn.ended' AND e.thread = ?1
         AND e.turn IS NOT NULL AND json_extract(e.data, '$.result') = 'error'
         AND NOT EXISTS (SELECT 1 FROM events a WHERE a.kind = 'attention.raised'
                         AND a.thread = e.thread AND a.turn = e.turn AND a.source = ?2
                         AND json_extract(a.data, '$.reason') = 'error')
         ORDER BY e.rowid",
    )?;
    let rows = st.query_map(params![thread, Source::Hub.to_string()], |r| {
        r.get::<_, i64>(0)
    })?;
    let mut turns: Vec<u64> = Vec::new();
    for t in rows {
        let t = u64::try_from(t?)?;
        if !turns.contains(&t) {
            turns.push(t);
        }
    }
    Ok(turns)
}

/// Tool calls, and tool results with `ok: false`, of turn `turn` of `thread` (structured
/// fields for the grading decision).
pub fn turn_tool_counts(conn: &Connection, thread: &str, turn: u64) -> anyhow::Result<(u32, u32)> {
    let mut st = conn.prepare_cached(
        "SELECT COALESCE(SUM(kind = 'agent.tool_call'), 0),
                COALESCE(SUM(kind = 'agent.tool_result' AND json_extract(data, '$.ok') = 0), 0)
         FROM events WHERE thread = ?1 AND turn = ?2
         AND kind IN ('agent.tool_call', 'agent.tool_result')",
    )?;
    let (calls, failures): (i64, i64) = st.query_row(params![thread, to_i64(turn)?], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    let clamp = |n: i64| u32::try_from(n.max(0)).unwrap_or(u32::MAX);
    Ok((clamp(calls), clamp(failures)))
}

// The per-kind lookups of one thread below are keyed on `events_kind_turn (kind, thread,
// turn)`. Without the `+` in `ORDER BY +seq`, SQLite (which has no statistics here) orders by
// walking the primary key `(thread, seq)` and tests every event of the thread on the way: one
// lookup read every event after the one it found, and all of them when there was none. Where
// several events of the kind may match, the subquery picks their `seq` (no payload read) and
// only the events it names are read in full.

/// [`last_of_kind`] in one turn.
const LAST_OF_KIND_IN_TURN: &str = concat!(
    "SELECT ",
    event_cols!(),
    " FROM events WHERE kind = ?1 AND thread = ?2 AND turn = ?3 ORDER BY +seq DESC LIMIT 1"
);

/// [`last_of_kind`] in any turn.
const LAST_OF_KIND: &str = concat!(
    "SELECT ",
    event_cols!(),
    " FROM events WHERE thread = ?2 AND seq = (SELECT seq FROM events
     WHERE kind = ?1 AND thread = ?2 ORDER BY +seq DESC LIMIT 1)"
);

/// [`first_of_kind`].
const FIRST_OF_KIND: &str = concat!(
    "SELECT ",
    event_cols!(),
    " FROM events WHERE thread = ?2 AND seq = (SELECT seq FROM events
     WHERE kind = ?1 AND thread = ?2 ORDER BY +seq LIMIT 1)"
);

/// [`last_final_text`] (`final` survives capping: only long strings move to blobs).
const LAST_FINAL_TEXT: &str = concat!(
    "SELECT ",
    event_cols!(),
    " FROM events WHERE kind = 'agent.text' AND thread = ?1 AND turn = ?2
     AND json_extract(data, '$.final') = 1 ORDER BY +seq DESC LIMIT 1"
);

/// [`latest_of_kind`].
const LATEST_OF_KIND: &str = concat!(
    "SELECT ",
    event_cols!(),
    " FROM events WHERE thread = ?2 AND seq IN (SELECT seq FROM events
     WHERE kind = ?1 AND thread = ?2 ORDER BY +seq DESC LIMIT ?3) ORDER BY seq DESC"
);

/// [`seqs_of_kind`].
const SEQS_OF_KIND: &str =
    "SELECT seq FROM events WHERE kind = ?1 AND thread = ?2 ORDER BY +seq DESC";

/// [`event_at`].
const EVENT_AT: &str = concat!(
    "SELECT ",
    event_cols!(),
    " FROM events WHERE thread = ?1 AND seq = ?2"
);

/// The latest event of kind `kind` of `thread` in turn `turn` (any turn when `None`), as
/// stored (possibly capped).
pub fn last_of_kind(
    conn: &Connection,
    thread: &str,
    turn: Option<u64>,
    kind: &str,
) -> anyhow::Result<Option<Event>> {
    let row = match turn {
        Some(turn) => conn
            .prepare_cached(LAST_OF_KIND_IN_TURN)?
            .query_row(params![kind, thread, to_i64(turn)?], row_to_raw),
        None => conn
            .prepare_cached(LAST_OF_KIND)?
            .query_row(params![kind, thread], row_to_raw),
    }
    .optional()?;
    row.map(|r| r.into_event()).transpose()
}

/// The first event of kind `kind` of `thread`, as stored (possibly capped).
pub fn first_of_kind(conn: &Connection, thread: &str, kind: &str) -> anyhow::Result<Option<Event>> {
    let mut st = conn.prepare_cached(FIRST_OF_KIND)?;
    let row = st.query_row(params![kind, thread], row_to_raw).optional()?;
    row.map(|r| r.into_event()).transpose()
}

/// The latest final `agent.text` of turn `turn` of `thread`, as stored (possibly capped).
pub fn last_final_text(
    conn: &Connection,
    thread: &str,
    turn: u64,
) -> anyhow::Result<Option<Event>> {
    let mut st = conn.prepare_cached(LAST_FINAL_TEXT)?;
    let row = st
        .query_row(params![thread, to_i64(turn)?], row_to_raw)
        .optional()?;
    row.map(|r| r.into_event()).transpose()
}

/// The turn of `thread` that message `msg_ref` started (its `turn.started` `ext.twr.ref`),
/// if one did.
pub fn turn_of_message(
    conn: &Connection,
    thread: &str,
    msg_ref: i64,
) -> anyhow::Result<Option<u64>> {
    let turn: Option<i64> = conn
        .prepare_cached(
            "SELECT turn FROM events WHERE kind = 'turn.started' AND thread = ?1
             AND json_extract(data, '$.ext.twr.ref') = ?2 ORDER BY seq DESC LIMIT 1",
        )?
        .query_row(params![thread, msg_ref], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(turn.map(u64::try_from).transpose()?)
}

/// Whether `thread`'s message `msg_ref` was taken back (`prompt.withdrawn` or
/// `turn.withdrawn`).
pub fn message_withdrawn(conn: &Connection, thread: &str, msg_ref: i64) -> anyhow::Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT 1 FROM events WHERE kind IN ('prompt.withdrawn', 'turn.withdrawn')
             AND thread = ?1 AND json_extract(data, '$.ref') = ?2 LIMIT 1",
        )?
        .query_row(params![thread, msg_ref], |_| Ok(()))
        .optional()?
        .is_some())
}

/// The turns of `thread` whose message the user took back (`turn.withdrawn`).
pub fn withdrawn_turns(conn: &Connection, thread: &str) -> anyhow::Result<HashSet<u64>> {
    let mut st = conn.prepare_cached(
        "SELECT turn FROM events WHERE kind = 'turn.withdrawn' AND thread = ?1
         AND turn IS NOT NULL",
    )?;
    let rows = st.query_map(params![thread], |r| r.get::<_, i64>(0))?;
    rows.map(|t| Ok(u64::try_from(t?)?)).collect()
}

/// The latest `limit` events of kind `kind` of `thread`, newest first, as stored (possibly
/// capped).
pub fn latest_of_kind(
    conn: &Connection,
    thread: &str,
    kind: &str,
    limit: usize,
) -> anyhow::Result<Vec<Event>> {
    let mut st = conn.prepare_cached(LATEST_OF_KIND)?;
    let rows = st.query_map(params![kind, thread, limit as i64], row_to_raw)?;
    rows.map(|r| r?.into_event()).collect()
}

/// The `seq` of every event of kind `kind` of `thread`, newest first: for reading them one at
/// a time ([`event_at`]) until enough are read.
pub fn seqs_of_kind(conn: &Connection, thread: &str, kind: &str) -> anyhow::Result<Vec<u64>> {
    let mut st = conn.prepare_cached(SEQS_OF_KIND)?;
    let rows = st.query_map(params![kind, thread], |r| r.get::<_, i64>(0))?;
    rows.map(|s| Ok(u64::try_from(s?)?)).collect()
}

/// The event `seq` of `thread`, as stored (possibly capped).
pub fn event_at(conn: &Connection, thread: &str, seq: u64) -> anyhow::Result<Option<Event>> {
    let mut st = conn.prepare_cached(EVENT_AT)?;
    let row = st
        .query_row(params![thread, to_i64(seq)?], row_to_raw)
        .optional()?;
    row.map(|r| r.into_event()).transpose()
}

/// The latest `limit` events of kind `kind` across every thread, newest first (event ids are
/// monotonic), as stored (possibly capped).
pub fn latest_of_kind_anywhere(
    conn: &Connection,
    kind: &str,
    limit: usize,
) -> anyhow::Result<Vec<Event>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT {EVENT_COLS} FROM events WHERE kind = ?1 ORDER BY id DESC LIMIT ?2"
    ))?;
    let rows = st.query_map(params![kind, limit as i64], row_to_raw)?;
    rows.map(|r| r?.into_event()).collect()
}

/// [`BlobStore`] over the hub database (its own connection).
#[derive(Debug)]
pub struct SqliteBlobStore {
    conn: Mutex<Connection>,
}

impl SqliteBlobStore {
    /// Opens a blob store on the database at `path` (the schema must exist).
    pub fn open(path: &Path) -> anyhow::Result<SqliteBlobStore> {
        Ok(SqliteBlobStore {
            conn: Mutex::new(connect(path)?),
        })
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| anyhow!("blob store poisoned"))
    }
}

impl BlobStore for SqliteBlobStore {
    fn put(&self, bytes: &[u8]) -> anyhow::Result<String> {
        let r = blob_ref(bytes);
        self.lock()?.execute(
            "INSERT OR IGNORE INTO blobs (hash, bytes) VALUES (?1, ?2)",
            params![r, bytes],
        )?;
        Ok(r)
    }

    fn get(&self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let conn = self.lock()?;
        get_blob(&conn, blob_ref)
    }
}

/// A [`BlobStore`] that only records puts, so an append can commit its blobs in the same
/// transaction as the event.
#[derive(Debug, Default)]
pub struct StagedBlobs {
    staged: Mutex<Vec<(String, Vec<u8>)>>,
}

impl StagedBlobs {
    /// Number of staged blobs.
    pub fn len(&self) -> usize {
        self.staged.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether nothing is staged.
    pub fn is_empty(&self) -> bool {
        self.staged
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Takes the staged blobs.
    pub fn take(&self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut *self.staged.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl BlobStore for StagedBlobs {
    fn put(&self, bytes: &[u8]) -> anyhow::Result<String> {
        let r = blob_ref(bytes);
        let mut staged = self.staged.lock().map_err(|_| anyhow!("poisoned"))?;
        if !staged.iter().any(|(k, _)| *k == r) {
            staged.push((r.clone(), bytes.to_vec()));
        }
        Ok(r)
    }

    fn get(&self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let staged = self.staged.lock().map_err(|_| anyhow!("poisoned"))?;
        Ok(staged
            .iter()
            .find(|(k, _)| k == blob_ref)
            .map(|(_, b)| b.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::event::{AgentText, SessionPayload};

    fn ev(thread: &str, seq: u64, body: EventBody) -> Event {
        Event::new(thread, seq, Some(1), Source::Hub, body)
    }

    fn text(t: &str) -> EventBody {
        EventBody::AgentText(AgentText {
            text: t.into(),
            is_final: true,
            ..Default::default()
        })
    }

    /// The lines a replay writes, with their `data` as stored, read back as exactly the items
    /// serializing their events gives: for events of several kinds, control or not, and for
    /// rows written otherwise (`data` over several lines, which goes through its type and
    /// comes out on one; another offset and more digits in `ts`; a kind this build does not
    /// know).
    #[test]
    fn stream_lines_read_as_the_items_they_stand_for() {
        use twr_core::event::{AttentionRaised, TaskUpdated};
        use twr_core::proto::{ServerMessage, StreamItem, StreamKind};
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("twr.db")).unwrap();
        let bodies = vec![
            EventBody::ThreadStarted(SessionPayload::default()),
            text("hi \u{1F600} \"quoted\"\n\ttab"),
            EventBody::TaskUpdated(TaskUpdated {
                task_id: "b1".into(),
                running: true,
                description: "tests".into(),
                ext: serde_json::json!({"claude": {"n": 1.5}})
                    .as_object()
                    .cloned(),
                ..Default::default()
            }),
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "a1".into(),
                summary: "rm -rf target?".into(),
                options: vec!["yes".into(), "no".into()],
                ..Default::default()
            }),
            EventBody::ThreadStopped(SessionPayload::default()),
        ];
        let mut seq = 0;
        for body in bodies {
            seq += 1;
            let control = body.is_control();
            let mut e = ev("t1", seq, body);
            e.turn = (seq % 2 == 0).then_some(seq);
            e.raw = (seq == 2).then(|| "sha256:00".to_string());
            s.append(&e, control, AppendExtras::default()).unwrap();
        }
        for (data, ts, kind) in [
            (
                "{\n  \"task_id\": \"b2\",\n  \"running\": false\n}",
                "2026-10-20T03:04:05.123Z",
                "task.updated",
            ),
            (
                r#"{"session_id":"s9"}"#,
                "2026-10-20T05:04:05.123456+02:00",
                "thread.stopped",
            ),
            (
                r#"{"later":[1,{"x":null}]}"#,
                "2026-10-20T03:04:05Z",
                "thread.from_later",
            ),
        ] {
            seq += 1;
            s.conn
                .execute(
                    "INSERT INTO events (thread, seq, id, v, ts, turn, source, kind, data, raw)
                     VALUES ('t1', ?1, ?2, 1, ?3, NULL, 'user:tui', ?4, ?5, NULL)",
                    params![seq as i64, format!("id-{seq}"), ts, kind, data],
                )
                .unwrap();
            s.conn
                .execute(
                    "INSERT INTO control (thread, seq) VALUES ('t1', ?1)",
                    params![seq as i64],
                )
                .unwrap();
        }
        let read = |line: &[u8]| -> StreamItem {
            assert_eq!(line.last(), Some(&b'\n'));
            assert!(!line[..line.len() - 1].contains(&b'\n'), "one line");
            match serde_json::from_slice(line).unwrap() {
                ServerMessage::Stream(item) => *item,
                ServerMessage::Response(r) => panic!("{r:?}"),
            }
        };
        let through_type = |item: StreamItem| -> StreamItem {
            serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap()
        };
        let items = read_control(&s.conn, 0, 100).unwrap();
        let lines = read_control_lines(&s.conn, 0, 100).unwrap();
        assert_eq!(lines.len(), items.len());
        assert!(items.len() >= 5, "{items:?}");
        for ((g, event), (lg, line)) in items.into_iter().zip(&lines) {
            assert_eq!(g, *lg);
            let item = StreamItem {
                stream: StreamKind::Control,
                gseq: Some(g),
                event,
            };
            assert_eq!(read(line), through_type(item));
        }
        let events = read_thread_events(&s.conn, "t1", 0, 100).unwrap();
        let lines = read_thread_lines(&s.conn, "t1", 0, 100).unwrap();
        assert_eq!(lines.len(), seq as usize);
        assert_eq!(events.len(), lines.len());
        for (event, (lseq, line)) in events.into_iter().zip(&lines) {
            assert_eq!(event.seq, *lseq);
            let item = StreamItem {
                stream: StreamKind::Thread,
                gseq: None,
                event,
            };
            assert_eq!(read(line), through_type(item));
        }
        // From a seq on, at most a limit.
        let some = read_thread_lines(&s.conn, "t1", 2, 3).unwrap();
        assert_eq!(some.iter().map(|(q, _)| *q).collect::<Vec<_>>(), [3, 4, 5]);
    }

    #[test]
    fn append_load_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twr.db");
        let mut s = Store::open(&path).unwrap();
        let a = ev("t1", 1, EventBody::ThreadStarted(SessionPayload::default()));
        let b = ev("t1", 2, text("hi"));
        let c = ev("t2", 1, EventBody::ThreadStopped(SessionPayload::default()));
        assert_eq!(
            s.append(&a, true, AppendExtras::default()).unwrap(),
            Some(1)
        );
        assert_eq!(s.append(&b, false, AppendExtras::default()).unwrap(), None);
        assert_eq!(
            s.append(&c, true, AppendExtras::default()).unwrap(),
            Some(2)
        );
        assert_eq!(s.load_all().unwrap(), vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(
            s.load_all_with_gseq().unwrap(),
            vec![
                (a.clone(), Some(1)),
                (b.clone(), None),
                (c.clone(), Some(2))
            ]
        );
        assert_eq!(
            read_thread_events(s.conn(), "t1", 1, 10).unwrap(),
            vec![b.clone()]
        );
        assert_eq!(read_control(s.conn(), 0, 10).unwrap(), vec![(1, a), (2, c)]);
        assert_eq!(control_head(s.conn()).unwrap(), 2);
        assert_eq!(thread_head(s.conn(), "t1").unwrap(), 2);
        // Duplicate seq is rejected and leaves nothing behind.
        let dup = ev("t1", 2, text("again"));
        assert!(s.append(&dup, true, AppendExtras::default()).is_err());
        assert_eq!(control_head(s.conn()).unwrap(), 2);
        drop(s);
        // Reopening keeps the schema and data.
        let s = Store::open(&path).unwrap();
        assert_eq!(s.load_all().unwrap().len(), 3);
        let v: i32 = s
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let mode: String = s
            .conn()
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    /// The per-kind lookups find the newest (or first) match, also when a later turn or
    /// another thread has events of the kind, and only final texts count as replies.
    #[test]
    fn kind_lookups_read_the_matching_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("twr.db")).unwrap();
        let at = |thread: &str, seq: u64, turn: u64, body: EventBody| {
            Event::new(thread, seq, Some(turn), Source::Hub, body)
        };
        let partial = |t: &str| {
            EventBody::AgentText(AgentText {
                text: t.into(),
                ..Default::default()
            })
        };
        let started = || EventBody::ThreadStarted(SessionPayload::default());
        let log = [
            at("t1", 1, 1, started()),
            at("t1", 2, 1, text("one")),
            at("t1", 3, 1, partial("one, more")),
            at("t1", 4, 2, started()),
            at("t1", 5, 2, partial("two")),
            at("t2", 1, 1, text("other")),
            at("t1", 6, 3, started()),
            at("t1", 7, 3, text("three")),
            at("t1", 8, 3, text("three, again")),
        ];
        for e in &log {
            s.append(e, false, AppendExtras::default()).unwrap();
        }
        let c = s.conn();
        let seq = |e: Option<Event>| e.map(|e| e.seq);
        let seqs = |es: Vec<Event>| es.into_iter().map(|e| e.seq).collect::<Vec<_>>();
        let kind = "thread.started";
        assert_eq!(seq(last_of_kind(c, "t1", None, kind).unwrap()), Some(6));
        assert_eq!(seq(last_of_kind(c, "t1", Some(2), kind).unwrap()), Some(4));
        assert_eq!(seq(last_of_kind(c, "t1", Some(9), kind).unwrap()), None);
        assert_eq!(seq(last_of_kind(c, "t3", None, kind).unwrap()), None);
        assert_eq!(seq(first_of_kind(c, "t1", kind).unwrap()), Some(1));
        assert_eq!(seq(first_of_kind(c, "t2", kind).unwrap()), None);
        assert_eq!(seq(last_final_text(c, "t1", 1).unwrap()), Some(2));
        assert_eq!(seq(last_final_text(c, "t1", 2).unwrap()), None);
        assert_eq!(seq(last_final_text(c, "t1", 3).unwrap()), Some(8));
        assert_eq!(seqs(latest_of_kind(c, "t1", kind, 2).unwrap()), [6, 4]);
        assert_eq!(
            seqs(latest_of_kind(c, "t1", "agent.text", 9).unwrap()),
            [8, 7, 5, 3, 2]
        );
        assert_eq!(seqs_of_kind(c, "t1", kind).unwrap(), [6, 4, 1]);
        assert_eq!(event_at(c, "t1", 7).unwrap(), Some(log[7].clone()));
        assert_eq!(event_at(c, "t1", 9).unwrap(), None);
    }

    /// Every per-kind lookup of one thread goes through `events_kind_turn` (then reads the
    /// events it picked by primary key), never through a walk of the thread's events.
    #[test]
    fn kind_lookups_use_the_kind_index() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("twr.db")).unwrap();
        for sql in [
            LAST_OF_KIND_IN_TURN,
            LAST_OF_KIND,
            FIRST_OF_KIND,
            LAST_FINAL_TEXT,
            LATEST_OF_KIND,
            SEQS_OF_KIND,
        ] {
            let mut st = s
                .conn()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let args = ["k", "t", "1"].into_iter().take(st.parameter_count());
            let plan: Vec<String> = st
                .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let plan = plan.join("\n");
            assert!(
                plan.contains("USING INDEX events_kind_turn (kind=? AND thread=?"),
                "{sql}:\n{plan}"
            );
            assert!(!plan.contains("SCAN "), "{sql}:\n{plan}");
            assert!(!plan.contains("(thread=?)"), "{sql}:\n{plan}");
        }
    }

    #[test]
    fn triggers_reject_update_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twr.db");
        let mut s = Store::open(&path).unwrap();
        s.append(&ev("t1", 1, text("x")), true, AppendExtras::default())
            .unwrap();
        let conn = connect(&path).unwrap();
        for sql in [
            "UPDATE events SET data = '{}'",
            "DELETE FROM events",
            "UPDATE control SET seq = 9",
            "DELETE FROM control",
        ] {
            let err = conn.execute(sql, []).unwrap_err().to_string();
            assert!(err.contains("append-only"), "{sql}: {err}");
        }
        assert_eq!(s.load_all().unwrap().len(), 1);
    }

    #[test]
    fn blobs_and_dequeue_commit_with_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twr.db");
        let mut s = Store::open(&path).unwrap();
        let id = s
            .enqueue_prompt("t1", "hello", TurnFrom::User, None, &Source::Hub, 0)
            .unwrap();
        assert_eq!(s.pending_count("t1").unwrap(), 1);
        assert_eq!(s.threads_with_pending().unwrap(), vec!["t1".to_string()]);
        let staged = StagedBlobs::default();
        let r = staged.put(b"big").unwrap();
        let blobs = staged.take();
        s.append(
            &ev("t1", 1, text("x")),
            true,
            AppendExtras {
                blobs: &blobs,
                dequeue: Some(id),
            },
        )
        .unwrap();
        assert_eq!(s.pending_count("t1").unwrap(), 0);
        let bs = SqliteBlobStore::open(&path).unwrap();
        assert_eq!(bs.get(&r).unwrap(), Some(b"big".to_vec()));
        // Ranged reads.
        let conn = s.conn();
        assert_eq!(
            get_blob_chunk(conn, &r, 0, 2).unwrap(),
            Some((3, b"bi".to_vec()))
        );
        assert_eq!(
            get_blob_chunk(conn, &r, 2, 10).unwrap(),
            Some((3, b"g".to_vec()))
        );
        assert_eq!(
            get_blob_chunk(conn, &r, 3, 10).unwrap(),
            Some((3, Vec::new()))
        );
        assert_eq!(get_blob_chunk(conn, "sha256:none", 0, 1).unwrap(), None);
        // A failed append rolls back its dequeue and blobs.
        let id2 = s
            .enqueue_prompt("t1", "again", TurnFrom::User, None, &Source::Hub, 7)
            .unwrap();
        let staged = StagedBlobs::default();
        let r2 = staged.put(b"other").unwrap();
        let blobs = staged.take();
        let dup = ev("t1", 1, text("dup"));
        assert!(s
            .append(
                &dup,
                true,
                AppendExtras {
                    blobs: &blobs,
                    dequeue: Some(id2),
                },
            )
            .is_err());
        assert_eq!(s.pending_count("t1").unwrap(), 1);
        assert_eq!(bs.get(&r2).unwrap(), None);
        let p = s.next_pending("t1").unwrap().unwrap();
        assert_eq!((p.id, p.prompt.as_str()), (id2, "again"));
        assert_eq!(p.said_after, Some(7));
        assert_eq!(s.clear_pending("t1").unwrap(), 1);
        assert!(s.next_pending("t1").unwrap().is_none());
    }

    #[test]
    fn batches_commit_all_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twr.db");
        let mut s = Store::open(&path).unwrap();
        s.append(&ev("t1", 1, text("x")), true, AppendExtras::default())
            .unwrap();
        let prompt = |said_after| NewPrompt {
            thread: "t2".into(),
            prompt: "go".into(),
            from: TurnFrom::User,
            routed_by: Some("d1".into()),
            source: Source::Hub,
            said_after,
            user_send: false,
        };
        let a = ev("hub", 1, text("a"));
        let b = ev("hub", 2, text("b"));
        let committed = s
            .append_batch(AppendBatch {
                events: vec![(&a, true), (&b, false)],
                blobs: &[],
                dequeue: None,
                enqueue: vec![prompt(None), prompt(Some(1))],
                runner_ack: None,
                defer_sync: false,
                remote_cmds: &[],
                remote_done: &[],
                stop_requests: &[],
                drop_queues: &[],
            })
            .unwrap();
        assert_eq!(committed.gseqs, vec![Some(2), None]);
        assert_eq!(committed.queued.len(), 2);
        // `said_after: None` is the head after the batch's events.
        let p = s.next_pending("t2").unwrap().unwrap();
        assert_eq!(
            (p.said_after, p.routed_by.as_deref()),
            (Some(2), Some("d1"))
        );
        // A failing event (duplicate seq) rolls back the events before it, the dequeue and the
        // queued prompts.
        let c = ev("hub", 3, text("c"));
        let dup = ev("t1", 1, text("dup"));
        assert!(s
            .append_batch(AppendBatch {
                events: vec![(&c, true), (&dup, true)],
                blobs: &[],
                dequeue: Some(p.id),
                enqueue: vec![prompt(None)],
                runner_ack: None,
                defer_sync: false,
                remote_cmds: &[],
                remote_done: &[],
                stop_requests: &[],
                drop_queues: &[],
            })
            .is_err());
        assert_eq!(s.load_all().unwrap().len(), 3);
        assert_eq!(s.control_head().unwrap(), 2);
        assert_eq!(s.pending_count("t2").unwrap(), 2);
    }

    #[test]
    fn runner_acks_commit_with_their_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("twr.db")).unwrap();
        let ack = |rseq| RunnerAck {
            box_name: "gpu".into(),
            thread: "t1".into(),
            rseq,
        };
        assert_eq!(s.runner_ack("gpu", "t1").unwrap(), 0);
        let a = Event::new(
            "t1",
            1,
            None,
            Source::Hub,
            EventBody::AgentText(Default::default()),
        );
        let a1 = ack(1);
        s.append_batch(AppendBatch {
            events: vec![(&a, false)],
            runner_ack: Some(&a1),
            defer_sync: false,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.runner_ack("gpu", "t1").unwrap(), 1);
        // A failing batch (duplicate seq) leaves the watermark alone.
        let a2 = ack(2);
        assert!(s
            .append_batch(AppendBatch {
                events: vec![(&a, false)],
                runner_ack: Some(&a2),
                defer_sync: false,
                ..Default::default()
            })
            .is_err());
        assert_eq!(s.runner_ack("gpu", "t1").unwrap(), 1);
        // An ack alone (a frame that adds no event); watermarks never go back.
        s.append_batch(AppendBatch {
            runner_ack: Some(&a2),
            defer_sync: false,
            ..Default::default()
        })
        .unwrap();
        let a0 = ack(0);
        s.append_batch(AppendBatch {
            runner_ack: Some(&a0),
            defer_sync: false,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            s.runner_watermarks("gpu")
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![("t1".to_string(), 2)]
        );
        assert!(s.runner_watermarks("other").unwrap().is_empty());
    }

    /// Only a row the user sent counts as a send after a crash, by the head it was queued
    /// at, not by its `said_after` (which `say` caps at what the user had seen).
    #[test]
    fn only_the_users_own_sends_count_as_sent_since() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("twr.db")).unwrap();
        for seq in 1..=3 {
            s.append(&ev("t1", seq, text("x")), true, AppendExtras::default())
                .unwrap();
        }
        let prompt = |from, user_send| NewPrompt {
            thread: "t1".into(),
            prompt: "go".into(),
            from,
            routed_by: None,
            source: Source::User("tui".into()),
            said_after: Some(0),
            user_send,
        };
        let batch = |enqueue| AppendBatch {
            events: vec![],
            blobs: &[],
            dequeue: None,
            enqueue,
            runner_ack: None,
            defer_sync: false,
            remote_cmds: &[],
            remote_done: &[],
            stop_requests: &[],
            drop_queues: &[],
        };
        // A job's wake, the orchestrator's send, a user's prompt that waited for other
        // threads: queued after the crash at gseq 2, none of them the user's send.
        let others = s
            .append_batch(batch(vec![
                prompt(TurnFrom::Runtime, false),
                prompt(TurnFrom::Orchestrator, false),
                prompt(TurnFrom::User, false),
            ]))
            .unwrap();
        assert!(!s.user_sent_since("t1", 2).unwrap());
        assert!(!s.is_user_send(others.queued[2]).unwrap());
        // The user's send, its `said_after` capped before the crash.
        let sent = s
            .append_batch(batch(vec![prompt(TurnFrom::User, true)]))
            .unwrap();
        assert!(s.is_user_send(sent.queued[0]).unwrap());
        assert!(s.user_sent_since("t1", 2).unwrap());
        assert!(s.user_sent_since("t1", 3).unwrap());
        assert!(!s.user_sent_since("t1", 4).unwrap());
        assert!(!s.user_sent_since("t2", 0).unwrap());
    }

    #[test]
    fn schema_1_databases_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twr.db");
        {
            // A schema-1 database with a queued prompt.
            let conn = connect(&path).unwrap();
            let v1 = SCHEMA.replace(",\n    said_after INTEGER,\n    user_sent INTEGER", "");
            assert_ne!(v1, SCHEMA);
            conn.execute_batch(&v1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO pending_prompts (thread, prompt, from_, routed_by, source, queued_at)
                 VALUES ('t1', 'old', 'user', NULL, 'hub', '2026-01-01T00:00:00.000Z')",
                [],
            )
            .unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        let v: i32 = s
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // The old row has no arrival point (the relay guard treats it as unknown).
        let p = s.next_pending("t1").unwrap().unwrap();
        assert_eq!((p.prompt.as_str(), p.said_after), ("old", None));
        s.enqueue_prompt("t1", "new", TurnFrom::User, None, &Source::Hub, 3)
            .unwrap();
        s.clear_pending("t1").unwrap();
        drop(s);
        // Reopening a migrated database is a no-op.
        Store::open(&path).unwrap();
    }

    /// A schema-7 database gains `user_sent`; its queued rows count as no user's send (a crash
    /// hold they would have released stays until the user sends again).
    #[test]
    fn schema_7_databases_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twr.db");
        {
            let conn = connect(&path).unwrap();
            let v7 = SCHEMA.replace(",\n    user_sent INTEGER", "");
            assert_ne!(v7, SCHEMA);
            conn.execute_batch(&v7).unwrap();
            for schema in [REMOTE_OUTBOX, SCHEMA_4, SCHEMA_5, SCHEMA_6] {
                conn.execute_batch(schema).unwrap();
            }
            conn.pragma_update(None, "user_version", 7).unwrap();
            conn.execute(
                "INSERT INTO pending_prompts
                     (thread, prompt, from_, routed_by, source, queued_at, said_after)
                 VALUES ('t1', 'old', 'user', NULL, 'user:tui', '2026-01-01T00:00:00.000Z', 5)",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let p = s.next_pending("t1").unwrap().unwrap();
        assert_eq!((p.prompt.as_str(), p.said_after), ("old", Some(5)));
        assert!(!s.is_user_send(p.id).unwrap());
        assert!(!s.user_sent_since("t1", 0).unwrap());
        drop(s);
        Store::open(&path).unwrap();
    }

    #[test]
    fn stop_requests_and_spawn_intents_end_with_the_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("twr.db")).unwrap();
        s.enqueue_prompt("t1", "later", TurnFrom::User, None, &Source::Hub, 0)
            .unwrap();
        let stop = RemoteCmd {
            box_name: "gpu".into(),
            cmd_id: "stop:t1:1".into(),
            thread: "t1".into(),
            turn: None,
            op: r#"{"type":"stop"}"#.into(),
        };
        let batch = AppendBatch {
            events: vec![],
            blobs: &[],
            dequeue: None,
            enqueue: vec![],
            runner_ack: None,
            defer_sync: false,
            remote_cmds: std::slice::from_ref(&stop),
            remote_done: &[],
            stop_requests: &["t1".to_string()],
            drop_queues: &["t1".to_string(), "t2".to_string()],
        };
        assert_eq!(s.append_batch(batch).unwrap().dropped, 1);
        s.add_spawn_intent("t1", 42).unwrap();
        s.add_spawn_intent("t2", 43).unwrap();
        assert_eq!(s.stop_requests().unwrap(), vec!["t1".to_string()]);
        assert_eq!(s.remote_cmds("gpu").unwrap(), vec![stop]);
        // Another thread's closing event leaves them; the thread's own retires both.
        let other = Event::new(
            "t2",
            1,
            None,
            Source::Hub,
            EventBody::ThreadStopped(Default::default()),
        );
        s.append(&other, true, AppendExtras::default()).unwrap();
        assert_eq!(s.stop_requests().unwrap(), vec!["t1".to_string()]);
        assert_eq!(s.spawn_intents().unwrap(), vec![("t1".to_string(), 42)]);
        let closed = Event::new(
            "t1",
            1,
            None,
            Source::Hub,
            EventBody::ThreadCrashed(Default::default()),
        );
        s.append(&closed, true, AppendExtras::default()).unwrap();
        assert!(s.stop_requests().unwrap().is_empty());
        assert!(s.spawn_intents().unwrap().is_empty());
    }

    #[test]
    fn an_attach_process_lasts_until_the_detach() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("twr.db")).unwrap();
        let p = AttachProcess {
            pid: 4242,
            start_time: 987654,
            marker: "t1:3".into(),
            dir: Some("/w/t1".into()),
        };
        s.set_attach_process("t1", &p).unwrap();
        s.set_attach_process("t2", &p).unwrap();
        assert_eq!(s.attach_process("t1").unwrap(), Some(p.clone()));
        let ev = |thread: &str, seq: u64, body: EventBody| {
            Event::new(thread, seq, None, Source::Hub, body)
        };
        // Other events and other threads' detaches leave it.
        s.append(
            &ev("t1", 1, EventBody::ThreadStopped(Default::default())),
            true,
            AppendExtras::default(),
        )
        .unwrap();
        s.append(
            &ev("t2", 1, EventBody::ThreadDetached(Default::default())),
            true,
            AppendExtras::default(),
        )
        .unwrap();
        assert_eq!(s.attach_process("t1").unwrap(), Some(p));
        assert_eq!(s.attach_process("t2").unwrap(), None);
        s.append(
            &ev("t1", 2, EventBody::ThreadDetached(Default::default())),
            true,
            AppendExtras::default(),
        )
        .unwrap();
        assert_eq!(s.attach_process("t1").unwrap(), None);
    }

    #[test]
    fn old_remote_commands_expire() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("twr.db")).unwrap();
        let cmd = |id: &str| RemoteCmd {
            box_name: "gpu".into(),
            cmd_id: id.into(),
            thread: "t1".into(),
            turn: Some(1),
            op: "{}".into(),
        };
        let a = Event::new(
            "t1",
            1,
            Some(1),
            Source::Hub,
            EventBody::AgentText(Default::default()),
        );
        let cmds = [cmd("send:t1:1"), cmd("send:t1:2")];
        s.append_batch(AppendBatch {
            events: vec![(&a, false)],
            remote_cmds: &cmds,
            ..Default::default()
        })
        .unwrap();
        s.conn()
            .execute(
                "UPDATE remote_outbox SET created = '2000-01-01T00:00:00.000Z'
                 WHERE cmd_id = 'send:t1:1'",
                [],
            )
            .unwrap();
        let cutoff = Utc::now() - chrono::Duration::hours(1);
        assert_eq!(
            s.expired_remote_cmds("gpu", cutoff).unwrap(),
            vec![cmd("send:t1:1")]
        );
        // Listing retires nothing: the expired command goes with the event it implies.
        assert_eq!(s.remote_cmds("gpu").unwrap(), cmds.to_vec());
        let b = Event::new(
            "t1",
            2,
            Some(1),
            Source::Hub,
            EventBody::AgentText(Default::default()),
        );
        let done = [("gpu".to_string(), "send:t1:1".to_string())];
        // A batch that fails retires nothing either.
        assert!(s
            .append_batch(AppendBatch {
                events: vec![(&a, false)],
                remote_done: &done,
                ..Default::default()
            })
            .is_err());
        assert_eq!(s.remote_cmds("gpu").unwrap(), cmds.to_vec());
        s.append_batch(AppendBatch {
            events: vec![(&b, false)],
            remote_done: &done,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.remote_cmds("gpu").unwrap(), vec![cmd("send:t1:2")]);
        assert!(s.expired_remote_cmds("gpu", cutoff).unwrap().is_empty());
    }

    #[test]
    fn remote_commands_commit_with_their_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twr.db");
        {
            // A schema-2 database.
            let conn = connect(&path).unwrap();
            conn.execute_batch(&SCHEMA.replace(",\n    user_sent INTEGER", ""))
                .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        let cmd = |id: &str| RemoteCmd {
            box_name: "gpu".into(),
            cmd_id: id.into(),
            thread: "t1".into(),
            turn: Some(2),
            op: r#"{"type":"send"}"#.into(),
        };
        let a = Event::new(
            "t1",
            1,
            Some(2),
            Source::Hub,
            EventBody::AgentText(Default::default()),
        );
        let cmds = [cmd("send:t1:2"), cmd("x")];
        s.append_batch(AppendBatch {
            events: vec![(&a, false)],
            remote_cmds: &cmds,
            ..Default::default()
        })
        .unwrap();
        // A failing batch records none of its commands.
        let failing = [cmd("y")];
        assert!(s
            .append_batch(AppendBatch {
                events: vec![(&a, false)],
                remote_cmds: &failing,
                ..Default::default()
            })
            .is_err());
        assert_eq!(s.remote_cmds("gpu").unwrap(), cmds.to_vec());
        assert!(s.remote_cmds("other").unwrap().is_empty());
        assert!(s.remote_cmd_done("gpu", "x").unwrap());
        assert!(!s.remote_cmd_done("gpu", "x").unwrap());
        assert_eq!(s.remote_cmds("gpu").unwrap(), vec![cmd("send:t1:2")]);
    }
}
