//! The runner's durable spool: a private SQLite database at `<data>/runner-<box>.db` (0600).
//!
//! Tables:
//!
//! - `outbox(id INTEGER PRIMARY KEY, thread, rseq, frame)`, unique `(thread, rseq)`: every
//!   outbound `event` / `exited` frame, written (with its `rseq`, in the same transaction)
//!   before it is sent and deleted once the hub acks it. `id` is the global spool order; per
//!   thread it is also `rseq` order.
//! - `rseq_alloc(thread PRIMARY KEY, next)`: the next `rseq` per thread. It never goes back, so
//!   a runner restart continues each thread's sequence.
//! - `cmd_results(cmd_id PRIMARY KEY, result, ts)`: the `cmd_result` of every command, for
//!   dedup. `result` is NULL while the command runs; a NULL found when the spool opens means
//!   the runner died during the command, and it becomes an `interrupted` error. A NULL is never
//!   taken as leave to run the command again (its effect is unknown); a command cancelled
//!   before it began is forgotten instead ([`Spool::cmd_forget`]). Finished results are
//!   kept for 24 hours, at most the newest [`CMD_RESULTS_KEEP`]; those of durable commands
//!   (`send:` / `respond:` / `stop:` ids, [`twr_core::runner::is_durable_cmd_id`]), which the
//!   hub resends until answered, for [`twr_core::runner::RESULT_RETENTION`] however many other
//!   commands run meanwhile (the hub stops resending after
//!   [`twr_core::runner::RESEND_HORIZON`]), at most the newest [`DURABLE_RESULTS_KEEP`].
//! - `runtimes(thread PRIMARY KEY, runtime, pid, start_time, exe, min_start)`: the runtime
//!   process of every live session that reported one ([`RuntimeRecord`]). Written in the same transaction as the
//!   session's start frame and deleted in the same transaction as its `exited` frame (after a
//!   spool overflow, whose synthetic `exited` is recorded while the process still runs: once
//!   the process has exited, [`Spool::forget_runtime`]), so a row found when the spool opens is
//!   a runtime that may still run: the runner that started it died. [`crate::LocalBox::open`]
//!   kills it ([`crate::reap`]) and, once nothing of it is left, records its exit before
//!   anything else happens.
//! - `spawn_intents(thread PRIMARY KEY, runtime, min_start, ts)`: a session being started whose
//!   runtime has not reported its process yet ([`SpawnIntent`]). Written before the adapter
//!   launches anything and deleted in the transaction that records the runtime (or its exit),
//!   so a runner killed while a runtime initializes still leaves a trace: the next runner kills
//!   every process marked for the thread that started after `min_start` ([`crate::reap`]) and
//!   records the session's exit.
//! - `meta(key PRIMARY KEY, value)`: `protocol`, the runner protocol
//!   ([`twr_core::runner::PROTOCOL_VERSION`]) the outbox's frames were written under. A spool
//!   of another protocol (or of a runner from before the stamp) is used only when every
//!   frame it still holds decodes as this build's [`RunnerMsg`] (then it is stamped anew);
//!   otherwise [`Spool::open`] refuses it, naming how many frames and which protocol, so the
//!   runner fails where the user sees it instead of replaying frames the hub cannot read on
//!   every connection.
//!
//! Size bound: the frames in the outbox may take at most `max_bytes` (default
//! [`DEFAULT_MAX_BYTES`]). An append that would exceed it is refused ([`Appended::Full`])
//! unless forced; the caller (the box) then stops the runtime that produced it and records a
//! forced synthetic `exited` frame, so the hub sees the session end instead of a silent gap.
//!
//! Durability: WAL with `synchronous = FULL`, so a committed frame survives a crash of the
//! runner or the machine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use twr_core::runner::{
    cmd_errors, CmdError, CmdResult, RunnerMsg, DURABLE_CMD_PREFIXES, PROTOCOL_VERSION,
    RESULT_RETENTION,
};

/// Default bound of the outbox (256 MiB of frames).
pub const DEFAULT_MAX_BYTES: u64 = 256 << 20;

/// Finished command results kept at most.
pub const CMD_RESULTS_KEEP: usize = 1000;

/// How long finished command results are kept.
pub const CMD_RESULTS_TTL: Duration = Duration::from_secs(24 * 3600);

/// Finished results of durable commands kept at most (a safety bound: at human rates a
/// [`RESULT_RETENTION`] holds far fewer).
pub const DURABLE_RESULTS_KEEP: usize = 100_000;

/// One spooled frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Global spool order.
    pub id: i64,
    /// Thread.
    pub thread: String,
    /// Per-thread sequence number.
    pub rseq: u64,
    /// The JSON text frame.
    pub frame: String,
}

/// Outcome of [`Spool::append`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    /// Stored with this `rseq`.
    Stored {
        /// Allocated sequence number.
        rseq: u64,
    },
    /// Refused: the outbox is full (nothing was allocated).
    Full,
}

/// The process of a live runtime session (see the `runtimes` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRecord {
    /// Thread id.
    pub thread: String,
    /// Runtime (adapter) name.
    pub runtime: String,
    /// Process id (`ext.twr.pid`).
    pub pid: u32,
    /// Start time ([`crate::reap::process_start_time`]); without it the process is never
    /// killed.
    pub start_time: Option<u64>,
    /// Program (`ext.twr.exe`), checked against `argv[0]` when set.
    pub exe: Option<String>,
    /// A lower bound on the runtime's start time: the start time of the runner that started
    /// it (what its intent recorded, [`SpawnIntent::min_start`]). The next runner's recovery
    /// sweeps the thread's marked processes from there, and tells the recorded pid from a
    /// reused one by it when `start_time` is unknown.
    pub min_start: Option<u64>,
}

/// A session start in progress (see the `spawn_intents` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnIntent {
    /// Thread id.
    pub thread: String,
    /// Runtime (adapter) name.
    pub runtime: String,
    /// No process of the session started before this (the start time of the runner that
    /// recorded the intent, [`crate::reap::process_start_time`]); `None` when unknown (then
    /// nothing is killed).
    pub min_start: Option<u64>,
}

/// What an append also does to the `runtimes` table, in the same transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeChange {
    /// Nothing.
    None,
    /// The session's runtime process started (replaces the thread's row).
    Started(RuntimeRecord),
    /// The session ended (the frame is its `exited`): the thread's row goes.
    Ended,
}

/// What [`Spool::cmd_begin`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum CmdState {
    /// Unknown id: now recorded as running; execute it and call [`Spool::cmd_finish`].
    New,
    /// Seen before: the stored result.
    Done(CmdResult),
}

struct Inner {
    conn: Connection,
    /// Sum of `length(frame)` over the outbox.
    bytes: u64,
    /// Injected failures still to come (tests): appends, command-result writes.
    fail_appends: u32,
    fail_cmd_finish: u32,
}

/// The spool. All methods are synchronous and short; call them from any thread.
pub struct Spool {
    inner: Mutex<Inner>,
    /// Held (advisory lock) while the spool is open: one runner per box and data directory.
    _lock: std::fs::File,
    max_bytes: u64,
    path: PathBuf,
    interrupted: Vec<String>,
}

impl std::fmt::Debug for Spool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spool")
            .field("path", &self.path)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How long [`Spool::open`] waits for another runner to release the spool.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// Locks `<path>.lock` exclusively (waiting up to [`LOCK_WAIT`]).
fn lock_spool(path: &Path) -> anyhow::Result<std::fs::File> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    create_private(&lock_path)?;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "another runner is using {} (one runner per box and data directory)",
                path.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", lock_path.display()))
            }
        }
    }
}

/// The `meta` key of the spool's protocol stamp.
const PROTOCOL_KEY: &str = "protocol";

/// Checks the protocol stamp of the spool `conn` (at `path`, for errors) against this build's
/// [`PROTOCOL_VERSION`]: the same, nothing to do; another one (or none: a spool from before
/// the stamp, or a new one), every frame in the outbox must decode as this build's
/// [`RunnerMsg`], and then the spool is stamped with this build's protocol. A frame that does
/// not decode refuses the spool: the hub could not read it either, and the runner would send
/// it again on every connection. The error names the count and the protocol, and what to do.
fn check_protocol(conn: &Connection, path: &Path) -> anyhow::Result<()> {
    let stamp: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [PROTOCOL_KEY],
            |r| r.get(0),
        )
        .optional()?;
    let current = PROTOCOL_VERSION.to_string();
    if stamp.as_deref() == Some(current.as_str()) {
        return Ok(());
    }
    let (mut frames, mut unreadable) = (0u64, 0u64);
    {
        let mut st = conn.prepare("SELECT frame FROM outbox ORDER BY id")?;
        let mut rows = st.query([])?;
        while let Some(row) = rows.next()? {
            frames += 1;
            let frame: String = row.get(0)?;
            if serde_json::from_str::<RunnerMsg>(&frame).is_err() {
                unreadable += 1;
            }
        }
    }
    let was = match &stamp {
        Some(p) => format!("runner protocol {p}"),
        None => "a runner protocol from before spools recorded theirs".to_string(),
    };
    if unreadable > 0 {
        anyhow::bail!(
            "the runner spool {} holds {unreadable} unsent frame(s) (of {frames}) written under \
             {was}, which this twr (runner protocol {PROTOCOL_VERSION}) cannot read: run the twr \
             that wrote them until the hub has received them, or move the spool away (losing \
             those events)",
            path.display()
        );
    }
    if frames > 0 {
        tracing::info!(
            frames,
            from = %was,
            protocol = PROTOCOL_VERSION,
            "the spool's frames were written under another protocol and read fine: keeping them"
        );
    }
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![PROTOCOL_KEY, current],
    )?;
    Ok(())
}

/// Creates `path` with mode 0600 if missing and restricts it if others can read it.
fn create_private(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    twr_core::paths::restrict_file(path).with_context(|| format!("restricting {}", path.display()))
}

impl Spool {
    /// Opens (creating) the spool at `path` with an outbox bound of `max_bytes`.
    ///
    /// Takes an exclusive lock on `<path>.lock` first, waiting up to [`LOCK_WAIT`] for another
    /// runner of the same box to let go; two runners sharing a spool would interleave their
    /// sessions' frames.
    pub fn open(path: &Path, max_bytes: u64) -> anyhow::Result<Spool> {
        let lock = lock_spool(path)?;
        create_private(path)?;
        let conn = Connection::open(path)
            .with_context(|| format!("opening runner spool {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS outbox (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 thread TEXT NOT NULL,
                 rseq INTEGER NOT NULL,
                 frame TEXT NOT NULL,
                 UNIQUE(thread, rseq)
             );
             CREATE TABLE IF NOT EXISTS rseq_alloc (
                 thread TEXT PRIMARY KEY,
                 next INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS cmd_results (
                 cmd_id TEXT PRIMARY KEY,
                 result TEXT,
                 ts INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS cmd_results_ts ON cmd_results(ts);
             CREATE TABLE IF NOT EXISTS runtimes (
                 thread TEXT PRIMARY KEY,
                 runtime TEXT NOT NULL,
                 pid INTEGER NOT NULL,
                 start_time INTEGER,
                 exe TEXT,
                 min_start INTEGER
             );
             CREATE TABLE IF NOT EXISTS spawn_intents (
                 thread TEXT PRIMARY KEY,
                 runtime TEXT NOT NULL,
                 min_start INTEGER,
                 ts INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        check_protocol(&conn, path)?;
        // A spool from before `runtimes.min_start` gets the column (its rows have none).
        let has_min_start = conn
            .prepare("SELECT 1 FROM pragma_table_info('runtimes') WHERE name = 'min_start'")?
            .exists([])?;
        if !has_min_start {
            conn.execute_batch("ALTER TABLE runtimes ADD COLUMN min_start INTEGER")?;
        }
        // The WAL and shared-memory files take the database's mode; restrict them anyway in
        // case they predate it.
        for suffix in ["-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = twr_core::paths::restrict_file(Path::new(&p));
        }
        let bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(CAST(frame AS BLOB))), 0) FROM outbox",
            [],
            |r| r.get(0),
        )?;
        // Commands that were running when the previous runner died.
        let interrupted: Vec<String> = {
            let mut st = conn.prepare("SELECT cmd_id FROM cmd_results WHERE result IS NULL")?;
            let ids = st
                .query_map([], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            ids
        };
        for id in &interrupted {
            let r = interrupted_result(
                id,
                "the runner stopped while running this command; its effect is unknown",
            );
            conn.execute(
                "UPDATE cmd_results SET result = ?2, ts = ?3 WHERE cmd_id = ?1",
                params![id, serde_json::to_string(&r)?, now_ms()],
            )?;
        }
        if !interrupted.is_empty() {
            tracing::warn!(
                count = interrupted.len(),
                "commands were cut short by a runner crash; they answer `interrupted`"
            );
        }
        Ok(Spool {
            inner: Mutex::new(Inner {
                conn,
                bytes: bytes as u64,
                fail_appends: 0,
                fail_cmd_finish: 0,
            }),
            _lock: lock,
            max_bytes,
            path: path.to_path_buf(),
            interrupted,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where the spool lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Commands a crashed runner left unfinished (answered `interrupted`).
    pub fn interrupted(&self) -> &[String] {
        &self.interrupted
    }

    /// Allocates the next `rseq` of `thread` and stores `frame(rseq)`, in one transaction.
    /// Refused ([`Appended::Full`]) when it would take the outbox over its bound, unless
    /// `force`.
    pub fn append(
        &self,
        thread: &str,
        force: bool,
        frame: impl FnOnce(u64) -> String,
    ) -> anyhow::Result<Appended> {
        self.append_with(thread, force, frame, RuntimeChange::None)
    }

    /// [`Spool::append`] that also applies `change` to the `runtimes` table in the same
    /// transaction (nothing of it when the append is refused).
    pub fn append_with(
        &self,
        thread: &str,
        force: bool,
        frame: impl FnOnce(u64) -> String,
        change: RuntimeChange,
    ) -> anyhow::Result<Appended> {
        let mut g = self.lock();
        if g.fail_appends > 0 {
            g.fail_appends -= 1;
            anyhow::bail!("injected spool failure (append)");
        }
        let current = g.bytes;
        let tx = g
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let next: u64 = tx
            .query_row(
                "SELECT next FROM rseq_alloc WHERE thread = ?1",
                [thread],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|n| n as u64)
            .unwrap_or(1);
        let text = frame(next);
        let len = text.len() as u64;
        if !force && current.saturating_add(len) > self.max_bytes {
            return Ok(Appended::Full);
        }
        tx.execute(
            "INSERT INTO outbox (thread, rseq, frame) VALUES (?1, ?2, ?3)",
            params![thread, next as i64, text],
        )?;
        tx.execute(
            "INSERT INTO rseq_alloc (thread, next) VALUES (?1, ?2)
             ON CONFLICT(thread) DO UPDATE SET next = excluded.next",
            params![thread, (next + 1) as i64],
        )?;
        match change {
            RuntimeChange::None => {}
            RuntimeChange::Started(r) => {
                tx.execute(
                    "INSERT INTO runtimes (thread, runtime, pid, start_time, exe, min_start)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(thread) DO UPDATE SET runtime = excluded.runtime,
                         pid = excluded.pid, start_time = excluded.start_time,
                         exe = excluded.exe, min_start = excluded.min_start",
                    params![
                        thread,
                        r.runtime,
                        i64::from(r.pid),
                        r.start_time.map(|t| t as i64),
                        r.exe,
                        r.min_start.map(|t| t as i64)
                    ],
                )?;
                tx.execute("DELETE FROM spawn_intents WHERE thread = ?1", [thread])?;
            }
            RuntimeChange::Ended => {
                tx.execute("DELETE FROM runtimes WHERE thread = ?1", [thread])?;
                tx.execute("DELETE FROM spawn_intents WHERE thread = ?1", [thread])?;
            }
        }
        tx.commit()?;
        g.bytes += len;
        Ok(Appended::Stored { rseq: next })
    }

    /// The recorded runtime processes (see the `runtimes` table), by thread.
    pub fn runtimes(&self) -> anyhow::Result<Vec<RuntimeRecord>> {
        let g = self.lock();
        let mut st = g.conn.prepare(
            "SELECT thread, runtime, pid, start_time, exe, min_start FROM runtimes
             ORDER BY thread",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(RuntimeRecord {
                    thread: r.get(0)?,
                    runtime: r.get(1)?,
                    pid: r.get::<_, i64>(2)? as u32,
                    start_time: r.get::<_, Option<i64>>(3)?.map(|t| t as u64),
                    exe: r.get(4)?,
                    min_start: r.get::<_, Option<i64>>(5)?.map(|t| t as u64),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Forgets the runtime row and start intent of `thread` without recording a frame: its
    /// `exited` was recorded before its process ended (spool overflow), and now it has.
    pub fn forget_runtime(&self, thread: &str) -> anyhow::Result<()> {
        let mut g = self.lock();
        let tx = g
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM runtimes WHERE thread = ?1", [thread])?;
        tx.execute("DELETE FROM spawn_intents WHERE thread = ?1", [thread])?;
        tx.commit()?;
        Ok(())
    }

    /// Records that a session of `i.thread` is being started (replacing an older intent).
    pub fn intend(&self, i: &SpawnIntent) -> anyhow::Result<()> {
        let g = self.lock();
        g.conn.execute(
            "INSERT INTO spawn_intents (thread, runtime, min_start, ts) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(thread) DO UPDATE SET runtime = excluded.runtime,
                 min_start = excluded.min_start, ts = excluded.ts",
            params![i.thread, i.runtime, i.min_start.map(|t| t as i64), now_ms()],
        )?;
        Ok(())
    }

    /// Forgets the start intent of `thread` (the start failed before launching anything).
    pub fn forget_intent(&self, thread: &str) -> anyhow::Result<()> {
        self.lock()
            .conn
            .execute("DELETE FROM spawn_intents WHERE thread = ?1", [thread])?;
        Ok(())
    }

    /// Session starts whose runtime never reported its process (see `spawn_intents`).
    pub fn intents(&self) -> anyhow::Result<Vec<SpawnIntent>> {
        let g = self.lock();
        let mut st = g
            .conn
            .prepare("SELECT thread, runtime, min_start FROM spawn_intents ORDER BY thread")?;
        let rows = st
            .query_map([], |r| {
                Ok(SpawnIntent {
                    thread: r.get(0)?,
                    runtime: r.get(1)?,
                    min_start: r.get::<_, Option<i64>>(2)?.map(|t| t as u64),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Makes the next `appends` appends and the next `cmd_finishes` command-result writes fail
    /// (fault injection for tests).
    #[doc(hidden)]
    pub fn inject_failures(&self, appends: u32, cmd_finishes: u32) {
        let mut g = self.lock();
        g.fail_appends = appends;
        g.fail_cmd_finish = cmd_finishes;
    }

    /// Up to `limit` frames after spool position `after`, in spool order.
    pub fn rows_after(&self, after: i64, limit: usize) -> anyhow::Result<Vec<Row>> {
        let g = self.lock();
        let mut st = g.conn.prepare_cached(
            "SELECT id, thread, rseq, frame FROM outbox WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = st
            .query_map(params![after, limit as i64], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    thread: r.get(1)?,
                    rseq: r.get::<_, i64>(2)? as u64,
                    frame: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Deletes the frames of `thread` with `rseq` at or below `rseq` (a cumulative ack).
    /// Returns how many were deleted.
    pub fn ack(&self, thread: &str, rseq: u64) -> anyhow::Result<usize> {
        let mut g = self.lock();
        let freed: i64 = g.conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(CAST(frame AS BLOB))), 0) FROM outbox
             WHERE thread = ?1 AND rseq <= ?2",
            params![thread, rseq as i64],
            |r| r.get(0),
        )?;
        let n = g.conn.execute(
            "DELETE FROM outbox WHERE thread = ?1 AND rseq <= ?2",
            params![thread, rseq as i64],
        )?;
        g.bytes = g.bytes.saturating_sub(freed as u64);
        Ok(n)
    }

    /// Applies the hub's watermarks from a welcome: deletes what the hub already has, and
    /// makes sure new frames of each thread get an `rseq` above its watermark (the hub drops
    /// anything at or below it as a duplicate; this matters when the spool was lost or
    /// reset while the hub kept its watermarks).
    pub fn apply_watermarks(&self, watermarks: &BTreeMap<String, u64>) -> anyhow::Result<()> {
        let mut g = self.lock();
        let tx = g
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (thread, &wm) in watermarks {
            // Checked: the watermark is the hub's, and the next rseq must fit SQLite.
            let next = wm
                .checked_add(1)
                .and_then(|n| i64::try_from(n).ok())
                .ok_or_else(|| {
                    anyhow::anyhow!("the hub's watermark {wm} of {thread} is out of range")
                })?;
            tx.execute(
                "DELETE FROM outbox WHERE thread = ?1 AND rseq < ?2",
                params![thread, next],
            )?;
            tx.execute(
                "INSERT INTO rseq_alloc (thread, next) VALUES (?1, ?2)
                 ON CONFLICT(thread) DO UPDATE SET next = MAX(next, excluded.next)",
                params![thread, next],
            )?;
        }
        let bytes = tx_bytes(&tx)?;
        tx.commit()?;
        g.bytes = bytes;
        Ok(())
    }

    /// Highest spooled `rseq` per thread.
    pub fn pending(&self) -> anyhow::Result<BTreeMap<String, u64>> {
        let g = self.lock();
        let mut st = g
            .conn
            .prepare("SELECT thread, MAX(rseq) FROM outbox GROUP BY thread")?;
        let m = st
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok(m)
    }

    /// Number of spooled frames.
    pub fn len(&self) -> anyhow::Result<u64> {
        let g = self.lock();
        let n: i64 = g
            .conn
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Whether the outbox is empty.
    pub fn is_empty(&self) -> anyhow::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Bytes of spooled frames.
    pub fn bytes(&self) -> u64 {
        self.lock().bytes
    }

    /// The bound of the outbox.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The `rseq` the next frame of `thread` gets.
    pub fn next_rseq(&self, thread: &str) -> anyhow::Result<u64> {
        let g = self.lock();
        let n = g
            .conn
            .query_row(
                "SELECT next FROM rseq_alloc WHERE thread = ?1",
                [thread],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(n.map(|n| n as u64).unwrap_or(1))
    }

    /// Looks `cmd_id` up; unknown ids are recorded as running ([`CmdState::New`]).
    pub fn cmd_begin(&self, cmd_id: &str) -> anyhow::Result<CmdState> {
        let g = self.lock();
        let found: Option<Option<String>> = g
            .conn
            .query_row(
                "SELECT result FROM cmd_results WHERE cmd_id = ?1",
                [cmd_id],
                |r| r.get(0),
            )
            .optional()?;
        match found {
            Some(Some(text)) => Ok(CmdState::Done(serde_json::from_str(&text)?)),
            // Started and never finished (its result could not be recorded): the effect is
            // unknown, so it is never run again.
            Some(None) => Ok(CmdState::Done(interrupted_result(
                cmd_id,
                "the command ran but its result could not be recorded; its effect is unknown",
            ))),
            None => {
                g.conn.execute(
                    "INSERT INTO cmd_results (cmd_id, result, ts) VALUES (?1, NULL, ?2)",
                    params![cmd_id, now_ms()],
                )?;
                Ok(CmdState::New)
            }
        }
    }

    /// Forgets `cmd_id` while it is recorded as running ([`Spool::cmd_begin`]) and has not
    /// begun: a repeat of the id runs it. A finished result stays.
    pub fn cmd_forget(&self, cmd_id: &str) -> anyhow::Result<()> {
        let g = self.lock();
        g.conn.execute(
            "DELETE FROM cmd_results WHERE cmd_id = ?1 AND result IS NULL",
            [cmd_id],
        )?;
        Ok(())
    }

    /// The stored result of `cmd_id`, if it finished.
    pub fn cmd_result(&self, cmd_id: &str) -> anyhow::Result<Option<CmdResult>> {
        let g = self.lock();
        let found: Option<Option<String>> = g
            .conn
            .query_row(
                "SELECT result FROM cmd_results WHERE cmd_id = ?1",
                [cmd_id],
                |r| r.get(0),
            )
            .optional()?;
        match found.flatten() {
            Some(t) => Ok(Some(serde_json::from_str(&t)?)),
            None => Ok(None),
        }
    }

    /// Stores the result of a command and applies retention.
    pub fn cmd_finish(&self, result: &CmdResult) -> anyhow::Result<()> {
        let mut g = self.lock();
        if g.fail_cmd_finish > 0 {
            g.fail_cmd_finish -= 1;
            anyhow::bail!("injected spool failure (command result)");
        }
        let now = now_ms();
        g.conn.execute(
            "INSERT INTO cmd_results (cmd_id, result, ts) VALUES (?1, ?2, ?3)
             ON CONFLICT(cmd_id) DO UPDATE SET result = excluded.result, ts = excluded.ts",
            params![result.cmd_id, serde_json::to_string(result)?, now],
        )?;
        // Two classes: durable commands (kept for RESULT_RETENTION) and the rest.
        let durable = durable_sql();
        for (class, ttl, keep) in [
            (format!("NOT {durable}"), CMD_RESULTS_TTL, CMD_RESULTS_KEEP),
            (durable, RESULT_RETENTION, DURABLE_RESULTS_KEEP),
        ] {
            g.conn.execute(
                &format!(
                    "DELETE FROM cmd_results WHERE result IS NOT NULL AND {class} AND (ts < ?1
                     OR cmd_id IN (SELECT cmd_id FROM cmd_results
                         WHERE result IS NOT NULL AND {class}
                         ORDER BY ts DESC, rowid DESC LIMIT -1 OFFSET ?2))"
                ),
                params![now - ttl.as_millis() as i64, keep as i64],
            )?;
        }
        Ok(())
    }

    /// Number of stored command results (tests).
    pub fn cmd_results_len(&self) -> anyhow::Result<u64> {
        let g = self.lock();
        let n: i64 = g
            .conn
            .query_row("SELECT COUNT(*) FROM cmd_results", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Ages every stored command result by `by` (tests of the 24 h retention).
    #[doc(hidden)]
    pub fn age_cmd_results(&self, by: Duration) -> anyhow::Result<()> {
        let g = self.lock();
        g.conn.execute(
            "UPDATE cmd_results SET ts = ts - ?1",
            [by.as_millis() as i64],
        )?;
        Ok(())
    }
}

/// SQL condition on `cmd_id`: a durable command's id ([`DURABLE_CMD_PREFIXES`]).
fn durable_sql() -> String {
    let parts: Vec<String> = DURABLE_CMD_PREFIXES
        .iter()
        .map(|p| {
            format!(
                "substr(cmd_id, 1, {}) = '{}'",
                p.len(),
                p.replace('\'', "''")
            )
        })
        .collect();
    format!("({})", parts.join(" OR "))
}

/// An `interrupted` result for `cmd_id`.
pub fn interrupted_result(cmd_id: &str, why: &str) -> CmdResult {
    CmdResult::new(cmd_id, Err(CmdError::new(cmd_errors::INTERRUPTED, why)))
}

fn tx_bytes(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<u64> {
    let b: i64 = tx.query_row(
        "SELECT COALESCE(SUM(LENGTH(CAST(frame AS BLOB))), 0) FROM outbox",
        [],
        |r| r.get(0),
    )?;
    Ok(b as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &Path, max: u64) -> Spool {
        Spool::open(&dir.join("runner-b.db"), max).unwrap()
    }

    /// [`open`] without the per-commit fsync (`synchronous = OFF`), for tests that write
    /// thousands of rows to test retention, not durability: with `FULL` each commit waits for
    /// the disk, and on a loaded one (100+ ms per fsync) such a test takes minutes.
    fn open_unsynced(dir: &Path, max: u64) -> Spool {
        let s = open(dir, max);
        s.lock()
            .conn
            .pragma_update(None, "synchronous", "OFF")
            .unwrap();
        s
    }

    fn frame(thread: &str) -> impl FnOnce(u64) -> String + '_ {
        move |rseq| format!(r#"{{"type":"exited","thread":"{thread}","rseq":{rseq}}}"#)
    }

    #[test]
    fn rseq_is_per_thread_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = open(dir.path(), DEFAULT_MAX_BYTES);
            assert_eq!(
                s.append("a", false, frame("a")).unwrap(),
                Appended::Stored { rseq: 1 }
            );
            assert_eq!(
                s.append("a", false, frame("a")).unwrap(),
                Appended::Stored { rseq: 2 }
            );
            assert_eq!(
                s.append("b", false, frame("b")).unwrap(),
                Appended::Stored { rseq: 1 }
            );
            assert_eq!(s.ack("a", 2).unwrap(), 2);
        }
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        // Acked frames are gone, but the sequence continues.
        assert_eq!(s.next_rseq("a").unwrap(), 3);
        assert_eq!(
            s.append("a", false, frame("a")).unwrap(),
            Appended::Stored { rseq: 3 }
        );
        let rows = s.rows_after(0, 100).unwrap();
        let got: Vec<_> = rows.iter().map(|r| (r.thread.as_str(), r.rseq)).collect();
        assert_eq!(got, vec![("b", 1), ("a", 3)]);
        assert_eq!(s.pending().unwrap().get("a"), Some(&3));
    }

    #[test]
    fn watermarks_drop_and_raise() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        for _ in 0..3 {
            s.append("a", false, frame("a")).unwrap();
        }
        let mut wm = BTreeMap::new();
        wm.insert("a".to_string(), 2);
        wm.insert("z".to_string(), 40);
        s.apply_watermarks(&wm).unwrap();
        let rows = s.rows_after(0, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rseq, 3);
        assert_eq!(s.next_rseq("a").unwrap(), 4);
        // A thread the hub knows better than the spool: new frames go above its watermark.
        assert_eq!(s.next_rseq("z").unwrap(), 41);
        assert_eq!(s.bytes(), rows[0].frame.len() as u64);
        // A watermark with no next rseq SQLite can hold: an error (no panic), nothing changed.
        for bad in [u64::MAX, i64::MAX as u64] {
            let wm = BTreeMap::from([("a".to_string(), 1), ("b".to_string(), bad)]);
            assert!(s.apply_watermarks(&wm).is_err(), "{bad}");
            assert_eq!(s.rows_after(0, 100).unwrap().len(), 1);
            assert_eq!(s.next_rseq("a").unwrap(), 4);
        }
    }

    #[test]
    fn the_bound_refuses_unforced_appends() {
        let dir = tempfile::tempdir().unwrap();
        let one = frame("a")(1).len() as u64;
        let s = open(dir.path(), one * 2);
        assert!(matches!(
            s.append("a", false, frame("a")).unwrap(),
            Appended::Stored { .. }
        ));
        assert!(matches!(
            s.append("a", false, frame("a")).unwrap(),
            Appended::Stored { .. }
        ));
        assert_eq!(s.append("a", false, frame("a")).unwrap(), Appended::Full);
        // Nothing was allocated for the refused frame.
        assert_eq!(s.next_rseq("a").unwrap(), 3);
        assert_eq!(
            s.append("a", true, frame("a")).unwrap(),
            Appended::Stored { rseq: 3 }
        );
        s.ack("a", 3).unwrap();
        assert_eq!(s.bytes(), 0);
        assert!(s.is_empty().unwrap());
    }

    #[test]
    fn command_results_dedup_and_retention() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = open(dir.path(), DEFAULT_MAX_BYTES);
            assert_eq!(s.cmd_begin("c1").unwrap(), CmdState::New);
            let r = CmdResult::new("c1", Ok(serde_json::json!(1)));
            s.cmd_finish(&r).unwrap();
            assert_eq!(s.cmd_begin("c1").unwrap(), CmdState::Done(r));
            // Crash while c2 runs.
            assert_eq!(s.cmd_begin("c2").unwrap(), CmdState::New);
        }
        let s = open_unsynced(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(s.interrupted(), ["c2".to_string()]);
        let CmdState::Done(r) = s.cmd_begin("c2").unwrap() else {
            panic!("c2 should be done")
        };
        assert_eq!(r.error.unwrap().code, cmd_errors::INTERRUPTED);

        for i in 0..(CMD_RESULTS_KEEP + 5) {
            let id = format!("n{i}");
            s.cmd_begin(&id).unwrap();
            s.cmd_finish(&CmdResult::new(id, Ok(serde_json::Value::Null)))
                .unwrap();
        }
        assert_eq!(s.cmd_results_len().unwrap(), CMD_RESULTS_KEEP as u64);
        assert!(s.cmd_result("n0").unwrap().is_none());
        assert!(s
            .cmd_result(&format!("n{}", CMD_RESULTS_KEEP + 4))
            .unwrap()
            .is_some());
        s.age_cmd_results(CMD_RESULTS_TTL + Duration::from_secs(1))
            .unwrap();
        s.cmd_begin("fresh").unwrap();
        s.cmd_finish(&CmdResult::new("fresh", Ok(serde_json::Value::Null)))
            .unwrap();
        assert_eq!(s.cmd_results_len().unwrap(), 1);
    }

    #[test]
    fn durable_results_outlive_the_count_and_the_day() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_unsynced(dir.path(), DEFAULT_MAX_BYTES);
        let finish = |id: &str| {
            s.cmd_begin(id).unwrap();
            s.cmd_finish(&CmdResult::new(id, Ok(serde_json::json!(id))))
                .unwrap();
        };
        for id in ["send:t1:3", "respond:01J", "stop:t1:r1"] {
            finish(id);
        }
        // Many other commands (git snapshots, ...) and more than a day later: still there.
        for i in 0..(CMD_RESULTS_KEEP + 10) {
            finish(&format!("g{i}"));
        }
        s.age_cmd_results(CMD_RESULTS_TTL + Duration::from_secs(60))
            .unwrap();
        finish("later");
        for id in ["send:t1:3", "respond:01J", "stop:t1:r1"] {
            assert!(s.cmd_result(id).unwrap().is_some(), "{id} was forgotten");
            // A repeat is answered from the store, never run again.
            assert!(matches!(s.cmd_begin(id).unwrap(), CmdState::Done(_)));
        }
        assert!(s.cmd_result("g0").unwrap().is_none());
        assert_eq!(s.cmd_results_len().unwrap(), 4);
        // Past the retention they go too.
        s.age_cmd_results(RESULT_RETENTION).unwrap();
        finish("last");
        assert!(s.cmd_result("send:t1:3").unwrap().is_none());
        assert_eq!(s.cmd_results_len().unwrap(), 1);
    }

    #[test]
    fn an_unfinished_command_is_never_run_again() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(s.cmd_begin("c1").unwrap(), CmdState::New);
        // Its result could not be recorded (the row stays NULL): a repeat is `interrupted`.
        s.inject_failures(0, 1);
        assert!(s
            .cmd_finish(&CmdResult::new("c1", Ok(serde_json::Value::Null)))
            .is_err());
        let CmdState::Done(r) = s.cmd_begin("c1").unwrap() else {
            panic!("c1 must not run again")
        };
        assert_eq!(r.error.unwrap().code, cmd_errors::INTERRUPTED);
    }

    #[test]
    fn runtime_rows_follow_their_frames() {
        let dir = tempfile::tempdir().unwrap();
        let rec = RuntimeRecord {
            thread: "a".into(),
            runtime: "claude-code".into(),
            pid: 42,
            start_time: Some(7),
            exe: Some("claude".into()),
            min_start: Some(5),
        };
        {
            let s = open(dir.path(), DEFAULT_MAX_BYTES);
            s.append_with("a", false, frame("a"), RuntimeChange::Started(rec.clone()))
                .unwrap();
            // A failed append records nothing, runtime row included.
            s.inject_failures(1, 0);
            assert!(s
                .append_with("b", false, frame("b"), RuntimeChange::Started(rec.clone()))
                .is_err());
            assert_eq!(s.runtimes().unwrap(), vec![rec.clone()]);
        }
        // Survives a reopen (a crash).
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(s.runtimes().unwrap(), vec![rec]);
        s.append_with("a", true, frame("a"), RuntimeChange::Ended)
            .unwrap();
        assert!(s.runtimes().unwrap().is_empty());
        assert_eq!(s.next_rseq("a").unwrap(), 3);
    }

    #[test]
    fn spawn_intents_last_until_the_runtime_or_its_exit_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let intent = |t: &str| SpawnIntent {
            thread: t.into(),
            runtime: "claude-code".into(),
            min_start: Some(100),
        };
        {
            let s = open(dir.path(), DEFAULT_MAX_BYTES);
            for t in ["a", "b", "c"] {
                s.intend(&intent(t)).unwrap();
            }
            s.forget_intent("c").unwrap();
        }
        // Survive a crash.
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(s.intents().unwrap(), vec![intent("a"), intent("b")]);
        let rec = RuntimeRecord {
            thread: "a".into(),
            runtime: "claude-code".into(),
            pid: 42,
            start_time: Some(107),
            exe: None,
            min_start: Some(100),
        };
        // The runtime row replaces the intent; an exit clears it too.
        s.append_with("a", false, frame("a"), RuntimeChange::Started(rec))
            .unwrap();
        s.append_with("b", true, frame("b"), RuntimeChange::Ended)
            .unwrap();
        assert!(s.intents().unwrap().is_empty());
        assert_eq!(s.runtimes().unwrap().len(), 1);
    }

    #[test]
    fn one_runner_per_spool() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        let started = std::time::Instant::now();
        let e = Spool::open(&dir.path().join("runner-b.db"), DEFAULT_MAX_BYTES).unwrap_err();
        assert!(e.to_string().contains("another runner"), "{e:#}");
        assert!(started.elapsed() >= LOCK_WAIT);
        drop(s);
        open(dir.path(), DEFAULT_MAX_BYTES);
    }

    /// The spool records its protocol: a spool of another protocol (or of none, from before
    /// the stamp) is used and stamped anew when its frames decode, and refused, naming the
    /// count and the protocol, when one does not. An empty one is always stamped anew.
    #[test]
    fn a_spool_of_another_protocol_is_used_only_if_its_frames_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner-b.db");
        let stamp = |s: &Spool| -> Option<String> {
            s.lock()
                .conn
                .query_row("SELECT value FROM meta WHERE key = 'protocol'", [], |r| {
                    r.get(0)
                })
                .optional()
                .unwrap()
        };
        let restamp = |s: &Spool, v: Option<&str>| {
            let g = s.lock();
            match v {
                Some(v) => g
                    .conn
                    .execute("UPDATE meta SET value = ?1 WHERE key = 'protocol'", [v])
                    .unwrap(),
                None => g.conn.execute("DELETE FROM meta", []).unwrap(),
            };
        };
        // A new spool is stamped with this protocol, and reopens as is.
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(stamp(&s), Some(PROTOCOL_VERSION.to_string()));
        s.append("a", false, frame("a")).unwrap();
        drop(s);
        let mut s = open(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(s.len().unwrap(), 1);
        // An older protocol, or none, whose frames decode: kept and stamped anew.
        for old in [Some("5"), None] {
            restamp(&s, old);
            drop(s);
            s = open(dir.path(), DEFAULT_MAX_BYTES);
            assert_eq!(stamp(&s), Some(PROTOCOL_VERSION.to_string()));
            assert_eq!(s.len().unwrap(), 1);
        }
        // A frame this build cannot decode, under another protocol: refused, nothing changed.
        s.append("b", false, |rseq| {
            format!(r#"{{"type":"exited-v9","thread":"b","rseq":{rseq}}}"#)
        })
        .unwrap();
        restamp(&s, Some("5"));
        drop(s);
        let e = Spool::open(&path, DEFAULT_MAX_BYTES)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("holds 1 unsent frame(s) (of 2) written under runner protocol 5")
                && e.contains(&format!("(runner protocol {PROTOCOL_VERSION})"))
                && e.contains(&path.display().to_string()),
            "{e}"
        );
        let conn = Connection::open(&path).unwrap();
        conn.execute("DELETE FROM meta", []).unwrap();
        drop(conn);
        let e = Spool::open(&path, DEFAULT_MAX_BYTES)
            .unwrap_err()
            .to_string();
        assert!(e.contains("from before spools recorded theirs"), "{e}");
        // Under this protocol the spool is this build's own, and opens as is.
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('protocol', ?1)",
            [PROTOCOL_VERSION.to_string()],
        )
        .unwrap();
        drop(conn);
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(s.len().unwrap(), 2);
        // Once the unreadable frame is gone (acked by the twr that wrote it), another
        // protocol's empty spool is stamped anew.
        s.ack("b", 1).unwrap();
        s.ack("a", 1).unwrap();
        restamp(&s, Some("5"));
        drop(s);
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        assert_eq!(stamp(&s), Some(PROTOCOL_VERSION.to_string()));
    }

    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path(), DEFAULT_MAX_BYTES);
        s.append("a", false, frame("a")).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let p = dir.path().join(format!("runner-b.db{suffix}"));
            if let Ok(m) = std::fs::metadata(&p) {
                assert_eq!(m.permissions().mode() & 0o777, 0o600, "{}", p.display());
            }
        }
    }
}
