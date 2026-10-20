//! Reading the log: `thread.events`, `control.read`, blobs, and the control and thread
//! subscriptions (live broadcast, with catch-up from SQLite for subscribers that fall behind).

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{broadcast, watch};
use twr_core::event::*;
use twr_core::proto::RpcError;

use super::{internal, ControlItem, Hub, HubState, Inner};
use crate::store;

/// Rows fetched per catch-up read.
const CATCH_UP_BATCH: usize = 512;

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
    /// `thread.events`: events of a thread with `seq > since_seq`.
    pub fn thread_events(
        &self,
        query: &str,
        since_seq: u64,
        limit: usize,
    ) -> Result<(String, Vec<Event>), RpcError> {
        let id = self.resolve(query)?;
        let events = store::read_thread_events(&self.inner.reader(), &id, since_seq, limit)
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

    /// A whole blob.
    pub fn blob(&self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
        store::get_blob(&self.inner.reader(), blob_ref)
    }

    /// `blob.get`: up to `len` bytes of a blob from `offset`, plus its total size.
    pub fn blob_chunk(
        &self,
        blob_ref: &str,
        offset: u64,
        len: usize,
    ) -> anyhow::Result<Option<(u64, Vec<u8>)>> {
        store::get_blob_chunk(&self.inner.reader(), blob_ref, offset, len)
    }

    /// Subscribes to the control stream from `gseq > since`: the backlog from SQLite, then
    /// live items, gap-free and duplicate-free.
    pub fn subscribe_control(&self, since: u64) -> ControlSubscription {
        let core = self.inner.core();
        let rx = self.inner.control_tx.subscribe();
        let head = core.gseq;
        drop(core);
        ControlSubscription {
            hub: Arc::clone(&self.inner),
            rx,
            state: self.inner.state.subscribe(),
            last: since,
            head,
            backlog: VecDeque::new(),
            lines: VecDeque::new(),
        }
    }

    /// Subscribes to one thread's full log from `seq > since_seq`.
    pub fn subscribe_thread(
        &self,
        query: &str,
        since_seq: u64,
    ) -> Result<ThreadSubscription, RpcError> {
        let core = self.inner.core();
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
    last: u64,
    /// Everything up to here is committed and readable from SQLite.
    head: u64,
    backlog: VecDeque<(u64, Event)>,
    /// Stored items read as their lines ([`ControlSubscription::write_stored`]).
    lines: VecDeque<(u64, Vec<u8>)>,
}

impl ControlSubscription {
    /// The control head when the subscription was made.
    pub fn head(&self) -> u64 {
        self.head
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
