//! Reading the log: `thread.events`, `control.read`, `decisions.list`, blobs, and the control
//! and thread subscriptions (live broadcast, with catch-up from SQLite for subscribers that fall
//! behind).
//!
//! A control subscription from the start can begin with the state at the head instead of the
//! items before it (`snapshot`, [`ControlSubscription::snapshot`]): what a client keeps of the
//! control stream ([`twr_core::client_state::ClientFold`]), which the hub folds as it commits
//! each item (`Core::client`), taken with the head under the same core lock as the live
//! receiver, so that every item after the head comes once, live or from the store, and none
//! before it. One too large to send ([`SNAPSHOT_MAX`]) is not sent: the subscription then
//! delivers the items from the start, compacted when asked, as one without it does.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{broadcast, watch};
use twr_core::event::*;
use twr_core::proto::RpcError;

use super::{internal, ControlItem, Hub, HubState, Inner};
use crate::store;

/// Rows fetched per catch-up read.
const CATCH_UP_BATCH: usize = 512;

/// Rows a compacted backlog's selection reads per lock of the reader connection
/// ([`ControlSubscription::compact`]): other readers wait for one read at most.
const SELECT_BATCH: usize = 4096;

/// Most bytes of a snapshot a control subscription sends in its ack
/// ([`ControlSubscription::snapshot`]; `HubOptions::snapshot_max`): well under a frame
/// ([`twr_core::proto::MAX_FRAME`]). A larger one is not sent: the items are, from the start.
pub const SNAPSHOT_MAX: usize = 4 * 1024 * 1024;

/// Most `decision.made` items one `decisions.list` returns.
pub const DECISIONS_MAX: usize = 2000;

impl Hub {
    /// Filtered transcript, defaulting to the latest turn when no cursor is supplied.
    pub fn transcript_events(
        &self,
        query: &str,
        since: Option<u64>,
        turn: Option<u64>,
        limit: usize,
    ) -> Result<(String, u64, Option<u64>, Vec<Event>), RpcError> {
        let thread = self.inner.core().resolve(query)?;
        let reader = self.inner.reader();
        let (start, before, chosen) = if turn.is_some() || since.is_none() {
            store::transcript_bounds(&reader, &thread, turn).map_err(|e| {
                if e.to_string() == "turn not found" {
                    super::rpc(twr_core::proto::codes::NOT_FOUND, "turn not found")
                } else {
                    internal(e)
                }
            })?
        } else {
            (0, i64::MAX as u64, None)
        };
        let since = since.unwrap_or(start).max(start);
        let events = store::read_transcript_events(&reader, &thread, since, before, limit)
            .map_err(internal)?;
        Ok((thread, since, chosen, events))
    }
    /// `thread.events`: events of a thread with `seq > since_seq`, those archived too
    /// (`hub::prune`). `query` may also be a deleted thread's id.
    pub fn thread_events(
        &self,
        query: &str,
        since_seq: u64,
        limit: usize,
    ) -> Result<(String, Vec<Event>), RpcError> {
        // A deleted thread's own id reads its events too (read only: nothing else takes it),
        // before any name: a later thread may be named or aliased like that id.
        // (One lock for both: a deletion between them would make the id found by neither.)
        let id = {
            let core = self.inner.core();
            if core.registry.is_deleted(query) {
                query.to_string()
            } else {
                core.resolve(query)?
            }
        };
        // With those archived (`hub::prune`), merged by seq: `twr.db` read first, then the
        // archive (where a row a compaction moved meanwhile is by then).
        let hot = store::read_thread_events(&self.inner.reader(), &id, since_seq, limit)
            .map_err(internal)?;
        let archived = self.inner.archive_reader();
        let events = super::prune::merge_archived(hot, archived.as_ref(), &id, since_seq, limit)
            .map_err(internal)?;
        Ok((id, events))
    }

    /// `control.read`: control items with `gseq > since`, plus the current head.
    pub fn control_read(
        &self,
        since: u64,
        limit: usize,
    ) -> anyhow::Result<(Vec<(u64, Event)>, u64)> {
        let head = self.inner.core().gseq;
        let items = store::read_control(&self.inner.reader(), since, limit)?;
        Ok((items, head))
    }

    /// `decisions.list`: the newest `limit` (at most [`DECISIONS_MAX`]) `decision.made` control
    /// items with `gseq < before`, newest first, their payloads hydrated (a client folds them
    /// whole, as a snapshot's), and whether older ones are left.
    pub fn decisions_list(
        &self,
        before: u64,
        limit: usize,
    ) -> anyhow::Result<(Vec<(u64, Event)>, bool)> {
        let limit = limit.min(DECISIONS_MAX);
        let reader = self.inner.reader();
        let mut items = store::read_decisions(&reader, before, limit + 1)?;
        let more = items.len() > limit;
        items.truncate(limit);
        for (_, ev) in &mut items {
            if let Some(full) = super::append::hydrated_from_store(&reader, ev) {
                *ev = full;
            }
        }
        Ok((items, more))
    }

    /// A whole blob.
    pub fn blob(&self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
        // `twr.db` first, then the archive (a blob a compaction moved meanwhile is there).
        let hot = store::get_blob(&self.inner.reader(), blob_ref)?;
        match hot {
            Some(b) => Ok(Some(b)),
            None => match self.inner.archive_reader().as_ref() {
                Some(a) => store::get_blob(a, blob_ref),
                None => Ok(None),
            },
        }
    }

    /// `blob.get`: up to `len` bytes of a blob from `offset`, plus its total size.
    pub fn blob_chunk(
        &self,
        blob_ref: &str,
        offset: u64,
        len: usize,
    ) -> anyhow::Result<Option<(u64, Vec<u8>)>> {
        // `twr.db` first, then the archive (a blob a compaction moved meanwhile is there).
        let hot = store::get_blob_chunk(&self.inner.reader(), blob_ref, offset, len)?;
        match hot {
            Some(b) => Ok(Some(b)),
            None => match self.inner.archive_reader().as_ref() {
                Some(a) => store::get_blob_chunk(a, blob_ref, offset, len),
                None => Ok(None),
            },
        }
    }

    /// Subscribes to the control stream from `gseq > since`: the backlog from SQLite, then
    /// live items, gap-free and duplicate-free.
    pub fn subscribe_control(&self, since: u64) -> ControlSubscription {
        self.subscribe_control_with(since, false)
    }

    /// [`Hub::subscribe_control`], its backlog up to the head of now compacted with `compact`
    /// ([`twr_core::compact`]): [`ControlSubscription::compact`] selects it before anything
    /// is read.
    pub fn subscribe_control_with(&self, since: u64, compact: bool) -> ControlSubscription {
        let core = self.inner.core();
        let rx = self.inner.control_tx.subscribe();
        let head = core.gseq;
        // What the selection needs of the state at the head, taken with it.
        let compact = compact.then(|| Compact::at(head, &core.registry));
        drop(core);
        self.control_subscription(rx, since, head, compact, None)
    }

    /// [`Hub::subscribe_control_with`] that, with `snapshot` (from the start only: `since` 0,
    /// else a `bad_request`), begins with the state at the head instead of the items up to it
    /// ([`ControlSubscription::snapshot`]): the subscription goes on after the head. Taken with
    /// the head and the live receiver under one core lock; so is what it falls back to, the
    /// items from the start (compacted with `compact`), should the state be too large to send.
    pub fn subscribe_control_snapshot(
        &self,
        since: u64,
        compact: bool,
        snapshot: bool,
    ) -> Result<ControlSubscription, RpcError> {
        if !snapshot {
            return Ok(self.subscribe_control_with(since, compact));
        }
        if since != 0 {
            return Err(super::rpc(
                twr_core::proto::codes::BAD_REQUEST,
                "a snapshot is of the whole stream: ask for it with since 0",
            ));
        }
        let started = std::time::Instant::now();
        let core = self.inner.core();
        let rx = self.inner.control_tx.subscribe();
        let head = core.gseq;
        let fold = core.client.clone();
        let fallback = compact.then(|| Compact::at(head, &core.registry));
        drop(core);
        let snap = Snapshot {
            fold,
            fallback,
            taken: started.elapsed(),
        };
        Ok(self.control_subscription(rx, head, head, None, Some(snap)))
    }

    /// A control subscription on `rx` (taken with `head` under the core lock) of the items
    /// after `since`.
    fn control_subscription(
        &self,
        rx: broadcast::Receiver<ControlItem>,
        since: u64,
        head: u64,
        compact: Option<Compact>,
        snapshot: Option<Snapshot>,
    ) -> ControlSubscription {
        ControlSubscription {
            hub: Arc::clone(&self.inner),
            rx,
            state: self.inner.state.subscribe(),
            since,
            last: since,
            head,
            backlog: VecDeque::new(),
            lines: VecDeque::new(),
            compact,
            snapshot,
        }
    }

    /// Subscribes to one thread's full log from `seq > since_seq`.
    pub fn subscribe_thread(
        &self,
        query: &str,
        since_seq: u64,
    ) -> Result<ThreadSubscription, RpcError> {
        let core = self.inner.core();
        // A deleted thread's id names that thread, never a later one named like it: nothing
        // more comes for it (`thread.events` reads what it has).
        if core.registry.is_deleted(query) {
            return Err(super::rpc(
                twr_core::proto::codes::NOT_FOUND,
                format!("thread {query} is deleted"),
            ));
        }
        let thread = core.resolve(query)?;
        let rx = self.inner.events_tx.subscribe();
        let head = core.seqs.get(&thread).copied().unwrap_or(0);
        drop(core);
        Ok(ThreadSubscription {
            hub: Arc::clone(&self.inner),
            thread,
            rx,
            state: self.inner.state.subscribe(),
            last: since_seq,
            head,
            backlog: VecDeque::new(),
            lines: VecDeque::new(),
        })
    }
}

/// Appends the line a subscription writes for `event` (a control item with its `gseq`), with
/// its newline.
pub(crate) fn stream_line(
    buf: &mut Vec<u8>,
    gseq: Option<u64>,
    event: &Event,
) -> serde_json::Result<()> {
    /// [`twr_core::proto::StreamItem`], borrowing its event.
    #[derive(serde::Serialize)]
    struct Item<'a> {
        stream: twr_core::proto::StreamKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        gseq: Option<u64>,
        event: &'a Event,
    }
    let stream = match gseq {
        Some(_) => twr_core::proto::StreamKind::Control,
        None => twr_core::proto::StreamKind::Thread,
    };
    serde_json::to_writer(
        &mut *buf,
        &Item {
            stream,
            gseq,
            event,
        },
    )?;
    buf.push(b'\n');
    Ok(())
}

/// How many bytes `v` takes serialized (without writing them anywhere).
fn json_len(v: &serde_json::Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut n = Count(0);
    let _ = serde_json::to_writer(&mut n, v);
    n.0
}

/// Waits until `rx` satisfies `f`, without holding the (non-`Send`) watch guard.
pub(crate) async fn wait_state(
    rx: &mut watch::Receiver<HubState>,
    f: impl FnMut(&HubState) -> bool,
) {
    let _ = rx.wait_for(f).await;
}

/// What a subscription's live wait produced.
enum Wake<T> {
    Item(T),
    Lagged,
    Closed,
    /// The hub stopped and nothing is left in the channel.
    Drained,
}

/// Next live item, or [`Wake::Drained`] once the hub has stopped and the channel is empty.
async fn wake<T: Clone>(
    rx: &mut broadcast::Receiver<T>,
    state: &mut watch::Receiver<HubState>,
) -> Wake<T> {
    use broadcast::error::{RecvError, TryRecvError};
    tokio::select! {
        biased;
        r = rx.recv() => match r {
            Ok(v) => Wake::Item(v),
            Err(RecvError::Lagged(n)) => {
                tracing::debug!(skipped = n, "subscriber lagged; catching up from the store");
                Wake::Lagged
            }
            Err(RecvError::Closed) => Wake::Closed,
        },
        _ = wait_state(state, |s| *s == HubState::Stopped) => match rx.try_recv() {
            Ok(v) => Wake::Item(v),
            Err(TryRecvError::Lagged(_)) => Wake::Lagged,
            Err(_) => Wake::Drained,
        },
    }
}

/// A control stream subscription (see [`Hub::subscribe_control`]).
pub struct ControlSubscription {
    hub: Arc<Inner>,
    rx: broadcast::Receiver<ControlItem>,
    state: watch::Receiver<HubState>,
    since: u64,
    last: u64,
    /// Everything up to here is committed and readable from SQLite.
    head: u64,
    backlog: VecDeque<(u64, Event)>,
    /// Stored items read as their lines ([`ControlSubscription::write_stored`]).
    lines: VecDeque<(u64, Vec<u8>)>,
    /// The compacted backlog still to deliver, when it is one.
    compact: Option<Compact>,
    /// The state at the head, until [`ControlSubscription::snapshot`] took it.
    snapshot: Option<Snapshot>,
}

/// What a subscription with `snapshot` took under the core lock
/// ([`Hub::subscribe_control_snapshot`]).
struct Snapshot {
    /// The state at the head.
    fold: twr_core::client_state::ClientFold,
    /// The compacted backlog from the start, should the state not be sent (with `compact`).
    fallback: Option<Compact>,
    /// How long taking it took (the core lock waited for and held).
    taken: std::time::Duration,
}

/// The control items as the compacted backlog's selection reads them
/// ([`twr_core::compact::Row`]), those of `(0, upto]`: what the replay found as the hub
/// opened, then read from the store as later ones are asked for. A stored control item never
/// changes.
#[derive(Default)]
pub(super) struct CompactRows {
    rows: Vec<twr_core::compact::Row>,
    upto: u64,
}

impl CompactRows {
    /// The rows of the control items up to `upto`, in gseq order.
    pub(super) fn new(rows: Vec<twr_core::compact::Row>, upto: u64) -> CompactRows {
        CompactRows { rows, upto }
    }

    /// The rows known.
    #[cfg(test)]
    pub(super) fn rows(&self) -> &[twr_core::compact::Row] {
        &self.rows
    }

    /// The rows of `(0, head]`, those after the ones known read from the store by `read`
    /// (the rows after a gseq up to `head`, at most [`SELECT_BATCH`] of them).
    fn upto(
        &mut self,
        head: u64,
        mut read: impl FnMut(u64) -> anyhow::Result<Vec<twr_core::compact::Row>>,
    ) -> anyhow::Result<&[twr_core::compact::Row]> {
        while self.upto < head {
            let batch = read(self.upto)?;
            match batch.last() {
                Some(end) => self.upto = end.gseq,
                // Nothing more is committed.
                None => self.upto = head,
            }
            self.rows.extend(batch);
        }
        Ok(&self.rows[..self.rows.partition_point(|r| r.gseq <= head)])
    }
}

/// A compacted backlog ([`ControlSubscription::compact`]).
struct Compact {
    /// The control head it runs up to: once its items are delivered, the subscription goes
    /// on from here, in full.
    head: u64,
    /// The state at `head` the selection reads; `None` once it ran.
    at_head: Option<twr_core::compact::Head>,
    /// The gseqs of what it keeps, still to deliver.
    kept: VecDeque<u64>,
}

impl Compact {
    /// The backlog up to `head`, with what its selection needs of the state there (`registry`
    /// at `head`: taken under the same core lock).
    fn at(head: u64, registry: &twr_core::reduce::Registry) -> Compact {
        Compact {
            head,
            at_head: Some(twr_core::compact::Head::of(registry)),
            kept: VecDeque::new(),
        }
    }
}

impl ControlSubscription {
    /// The control head when the subscription was made.
    pub fn head(&self) -> u64 {
        self.head
    }

    /// Whether it was made with `compact` ([`Hub::subscribe_control_with`]), or falls back to
    /// a compacted backlog ([`ControlSubscription::snapshot`]).
    pub fn compacted(&self) -> bool {
        self.compact.is_some()
    }

    /// Where it starts: the items after this come (the head, for one that sends a snapshot).
    pub fn since(&self) -> u64 {
        self.since
    }

    /// The state at the head that a subscription with `snapshot` begins with, as JSON, once
    /// (`None` after, and for one without): serialized here, outside the core lock. One larger
    /// than `HubOptions::snapshot_max` ([`SNAPSHOT_MAX`]), or that does not serialize, is not
    /// sent (`None`): the subscription then delivers the items from the start instead, the
    /// compacted backlog prepared with it when it was asked with `compact`
    /// ([`ControlSubscription::compact`] selects it), every item otherwise.
    pub fn snapshot(&mut self) -> Option<serde_json::Value> {
        let snap = self.snapshot.take()?;
        let started = std::time::Instant::now();
        let value = serde_json::to_value(&snap.fold);
        drop(snap.fold);
        let bytes = value.as_ref().map_or(0, json_len);
        let us = (snap.taken + started.elapsed()).as_micros();
        match value {
            Ok(v) if bytes <= self.hub.snapshot_max => {
                twr_core::trace::mark_with(|| format!("hub: snapshot ({bytes} bytes, {us} us)"));
                Some(v)
            }
            r => {
                match r {
                    Ok(_) => tracing::info!(
                        bytes,
                        max = self.hub.snapshot_max,
                        "the control snapshot is too large to send; sending the items instead"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "cannot serialize the control snapshot; sending the items instead"
                    ),
                }
                twr_core::trace::mark_with(|| {
                    format!("hub: snapshot not sent ({bytes} bytes, {us} us)")
                });
                self.since = 0;
                self.last = 0;
                self.compact = snap.fallback;
                None
            }
        }
    }

    /// Selects the compacted backlog of a subscription made with `compact`, in a blocking
    /// task, outside the core lock: of the items up to its head as the selection reads them
    /// ([`CompactRows`], those not known yet read from the store without decoding them), it
    /// keeps what [`twr_core::compact::select`] does. Returns how many items it delivers
    /// before the live ones.
    pub async fn compact(&mut self) -> anyhow::Result<u64> {
        let Some(c) = self.compact.as_mut() else {
            anyhow::bail!("not a compacted subscription");
        };
        let Some(at_head) = c.at_head.take() else {
            return Ok(c.kept.len() as u64);
        };
        let (hub, since, head) = (Arc::clone(&self.hub), self.since, c.head);
        let kept = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u64>> {
            let started = std::time::Instant::now();
            let mut known = hub
                .compact_rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let rows = known.upto(head, |after| {
                store::read_control_rows(&hub.reader(), after, head, SELECT_BATCH)
            })?;
            let kept = twr_core::compact::select(rows, since, &at_head);
            tracing::debug!(
                items = rows.len(),
                kept = kept.len(),
                ms = started.elapsed().as_millis() as u64,
                "compacted control backlog selected"
            );
            Ok(kept)
        })
        .await??;
        c.kept = kept.into();
        Ok(c.kept.len() as u64)
    }

    /// Appends the line of the next item committed up to the head known to `buf`, read from
    /// the store as a line ([`store::read_control_lines`]); `Ok(false)` when the next one is
    /// still to come live. Never waits. What [`ControlSubscription::next`] read already goes
    /// first.
    pub fn write_stored(&mut self, buf: &mut Vec<u8>) -> anyhow::Result<bool> {
        loop {
            if let Some((g, ev)) = self.backlog.pop_front() {
                if g > self.last {
                    self.last = g;
                    stream_line(buf, Some(g), &ev)?;
                    return Ok(true);
                }
                continue;
            }
            if let Some((g, line)) = self.lines.pop_front() {
                if g > self.last {
                    self.last = g;
                    buf.extend_from_slice(&line);
                    return Ok(true);
                }
                continue;
            }
            if let Some(batch) = self.compacted_batch()? {
                let rows = store::read_control_lines_at(&self.hub.reader(), &batch)?;
                self.lines.extend(rows);
                continue;
            }
            if self.last < self.head {
                let rows =
                    store::read_control_lines(&self.hub.reader(), self.last, CATCH_UP_BATCH)?;
                if rows.is_empty() {
                    // Nothing more is committed than what we already delivered.
                    self.head = self.last;
                }
                self.lines.extend(rows);
                continue;
            }
            return Ok(false);
        }
    }

    /// The gseqs of the compacted backlog's next items to read, if any are left; once none
    /// is, the subscription goes on after its head, in full (`Ok(None)`). Call it only with
    /// what was read before delivered.
    fn compacted_batch(&mut self) -> anyhow::Result<Option<Vec<u64>>> {
        let Some(c) = self.compact.as_mut() else {
            return Ok(None);
        };
        if c.at_head.is_some() {
            anyhow::bail!("the compacted backlog was not selected");
        }
        if !c.kept.is_empty() {
            let n = c.kept.len().min(CATCH_UP_BATCH);
            return Ok(Some(c.kept.drain(..n).collect()));
        }
        // Everything up to its head is delivered, or needs not be.
        self.last = self.last.max(c.head);
        self.compact = None;
        Ok(None)
    }

    /// The next item committed up to the head known, read from the store; `Ok(None)` when
    /// the next one is still to come live. Never waits.
    fn next_stored(&mut self) -> anyhow::Result<Option<ControlItem>> {
        loop {
            if let Some((g, ev)) = self.backlog.pop_front() {
                if g > self.last {
                    self.last = g;
                    return Ok(Some((g, Arc::new(ev))));
                }
                continue;
            }
            if let Some(batch) = self.compacted_batch()? {
                let rows = store::read_control_events_at(&self.hub.reader(), &batch)?;
                self.backlog.extend(rows);
                continue;
            }
            if self.last < self.head {
                let rows = store::read_control(&self.hub.reader(), self.last, CATCH_UP_BATCH)?;
                if rows.is_empty() {
                    // Nothing more is committed than what we already delivered.
                    self.head = self.last;
                }
                self.backlog.extend(rows);
                continue;
            }
            return Ok(None);
        }
    }

    /// The next item; `Ok(None)` once the hub has shut down.
    pub async fn next(&mut self) -> anyhow::Result<Option<ControlItem>> {
        loop {
            if let Some(item) = self.next_stored()? {
                return Ok(Some(item));
            }
            match wake(&mut self.rx, &mut self.state).await {
                Wake::Item((g, ev)) => {
                    if g == self.last + 1 {
                        self.last = g;
                        return Ok(Some((g, ev)));
                    }
                    if g > self.last {
                        // A gap: everything up to g is committed, read it from the store.
                        self.head = g;
                    }
                }
                Wake::Lagged => self.head = store::control_head(&self.hub.reader())?,
                Wake::Closed | Wake::Drained => return Ok(None),
            }
        }
    }
}

/// A thread log subscription (see [`Hub::subscribe_thread`]).
pub struct ThreadSubscription {
    hub: Arc<Inner>,
    thread: String,
    rx: broadcast::Receiver<Arc<Event>>,
    state: watch::Receiver<HubState>,
    last: u64,
    head: u64,
    backlog: VecDeque<Event>,
    /// Stored events read as their lines ([`ThreadSubscription::write_stored`]).
    lines: VecDeque<(u64, Vec<u8>)>,
}

impl ThreadSubscription {
    /// The thread id.
    pub fn thread(&self) -> &str {
        &self.thread
    }

    /// The thread's last seq when the subscription was made.
    pub fn head(&self) -> u64 {
        self.head
    }

    /// Appends the line of the next event logged up to the head known to `buf`, read from
    /// the store as a line ([`store::read_thread_lines`]); `Ok(false)` when the next one is
    /// still to come live. Never waits. What [`ThreadSubscription::next`] read already goes
    /// first.
    pub fn write_stored(&mut self, buf: &mut Vec<u8>) -> anyhow::Result<bool> {
        loop {
            if let Some(ev) = self.backlog.pop_front() {
                if ev.seq > self.last {
                    self.last = ev.seq;
                    stream_line(buf, None, &ev)?;
                    return Ok(true);
                }
                continue;
            }
            if let Some((seq, line)) = self.lines.pop_front() {
                if seq > self.last {
                    self.last = seq;
                    buf.extend_from_slice(&line);
                    return Ok(true);
                }
                continue;
            }
            if self.last < self.head {
                let rows = store::read_thread_lines(
                    &self.hub.reader(),
                    &self.thread,
                    self.last,
                    CATCH_UP_BATCH,
                )?;
                if rows.is_empty() {
                    self.head = self.last;
                }
                self.lines.extend(rows);
                continue;
            }
            return Ok(false);
        }
    }

    /// The next event logged up to the head known, read from the store; `Ok(None)` when the
    /// next one is still to come live. Never waits.
    fn next_stored(&mut self) -> anyhow::Result<Option<Arc<Event>>> {
        loop {
            if let Some(ev) = self.backlog.pop_front() {
                if ev.seq > self.last {
                    self.last = ev.seq;
                    return Ok(Some(Arc::new(ev)));
                }
                continue;
            }
            if self.last < self.head {
                let rows = store::read_thread_events(
                    &self.hub.reader(),
                    &self.thread,
                    self.last,
                    CATCH_UP_BATCH,
                )?;
                if rows.is_empty() {
                    self.head = self.last;
                }
                self.backlog.extend(rows);
                continue;
            }
            return Ok(None);
        }
    }

    /// The next event; `Ok(None)` once the hub has shut down.
    pub async fn next(&mut self) -> anyhow::Result<Option<Arc<Event>>> {
        loop {
            if let Some(ev) = self.next_stored()? {
                return Ok(Some(ev));
            }
            match wake(&mut self.rx, &mut self.state).await {
                Wake::Item(ev) => {
                    if ev.thread != self.thread || ev.seq <= self.last {
                        continue;
                    }
                    if ev.seq == self.last + 1 {
                        self.last = ev.seq;
                        return Ok(Some(ev));
                    }
                    self.head = ev.seq;
                }
                Wake::Lagged => {
                    self.head = store::thread_head(&self.hub.reader(), &self.thread)?;
                }
                Wake::Closed | Wake::Drained => return Ok(None),
            }
        }
    }
}
