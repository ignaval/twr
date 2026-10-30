//! Reading the orchestrator's messages aloud (`speech.*`, `docs/read-aloud.md`): the speakers
//! attached to the hub (`twr speak`, the TUI's own), what each one is sent, and the user's
//! requests and controls for them. The hub sends; whether a message plays is the speaker's
//! (its live switch, its queue).
//!
//! - **Speakers** (`speech.attach`): kept in memory while their connection lasts, never
//!   recorded. One per device: a second speaker of a device attached meanwhile is `conflict`
//!   (it retries: that is its standby). Its connection closing detaches it and ends its feed.
//!   `speech.state` on that connection reports what it plays, for `speech.status`.
//! - **The feed** of a speaker: the orchestrator's thread, followed across switches and
//!   resets. The control stream is subscribed first, then the orchestrator thread read, then
//!   that thread subscribed (its head taken with the subscription): a switch at any point is
//!   seen at the read or as a control item after it. At attach, a thread the speaker has a
//!   watermark for goes on after it, what was said longer than `catchup_secs` ago counted in
//!   one `missed` (sent before the next message), the rest sent as `catchup`; one it has none
//!   for goes on from its head (nothing older). On a switch the feed first handles what the
//!   thread left has logged already (without waiting) and sends a text it still holds as its
//!   turn's reply (the switch ends that turn), then sends `orchestrator` and reads the new
//!   thread from its start (after the attach's watermark for it, if any), all of it live: the
//!   speaker was there while it was said, so nothing is aged into `missed`.
//!   A subscription that fails (a store read) is made again after a pause (1 s, doubling up
//!   to 30 s), after the last event handled: nothing repeated, nothing lost.
//! - **What is read:** the final `agent.text`s (never a delta, thinking, a tool call or its
//!   result) of conversation turns ([`speakable_turn`]: never a handoff or a compaction, and a
//!   turn whose `turn.started` cannot be read, or its `ext` from the blob store, is silent),
//!   the user's own only with `turn_from = mine`, none of a turn the user took back
//!   (`turn.withdrawn`) from then on (one sent before cannot be unsaid; one held is dropped). A
//!   text is a reply (the turn's last) or commentary (one before):
//!   Codex says which (`ext.codex.phase`); otherwise a speaker that reads both gets it at once
//!   as a `message`, and one that reads either kind alone gets it once the turn says
//!   (the next text of the turn: commentary; the turn's end: its reply). Each text is read
//!   whole from the blob store when it was capped (`truncated` when that fails), at most
//!   [`TEXT_MAX`] bytes.
//! - **Requests** (`speech.request`): the last `last` messages of a thread (the orchestrator's
//!   by default), or the one `seq` names (its final `agent.text`, or a delta of it), sent to
//!   the speakers asked (a name or device; all by default) as one batch: a `replace` and its
//!   messages, oldest first, with nothing else sent to that speaker in between (one sender at
//!   a time per speaker), so two requests never interleave.
//! - **Controls** (`speech.control`): stop, pause, resume, skip, live, voice, rate, sent to the
//!   speakers asked.
//!
//! Nothing any `speech.*` method does is recorded in the event log. Users only: the
//! orchestrator gets `unauthorized` (and the server lets neither it nor agents call them).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use twr_core::blob::{has_capped, hydrate_with};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::speech::{
    cap_text, clean, name_error, speakable_turn, turn_is_users, CtlOp, MsgKind, Origin, ReadKind,
    SpeakerStatus, SpeechAttachAck, SpeechAttachParams, SpeechControlParams, SpeechControlResult,
    SpeechItem, SpeechRequestParams, SpeechRequestResult, SpeechStateParams, SpeechStatus,
    TurnsRead, REQUEST_MAX, TEXT_MAX,
};

use super::{internal, rpc, Caller, ControlSubscription, Hub, ThreadSubscription};
use crate::store;

/// Items waiting for a speaker's connection, at most: past it the feed waits (no item is
/// lost), and so do requests and controls, up to [`SEND_WAIT`].
pub const QUEUE: usize = 1024;
/// How long a request or a control waits to be sent to one speaker (another batch being
/// sent to it, its queue full) before it leaves that speaker out.
pub const SEND_WAIT: Duration = Duration::from_secs(5);
/// Turns whose readability a feed remembers.
const TURNS_KEPT: usize = 256;
/// Final texts a request reads per page, and pages at most.
const PAGE: usize = 64;
const PAGES: usize = 20;
/// The longest `error` of a speaker's state kept, in characters.
const STATE_ERROR_MAX: usize = 300;
/// How long a feed whose subscription failed waits before it subscribes again, first and at
/// most (doubling in between).
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How long a feed reads on in the orchestrator it leaves for a new one, through what that
/// thread had when the switch was seen (`Feed::leave`): the rest is stored, so it comes at
/// once; this only bounds a store that stopped answering.
const LEAVE_WAIT: Duration = Duration::from_secs(10);

/// The speakers attached, by key (the order they attached in).
#[derive(Default)]
pub(super) struct Speakers {
    next_key: u64,
    next_batch: u64,
    by_key: BTreeMap<u64, Speaker>,
}

/// An attached speaker.
struct Speaker {
    device: String,
    name: String,
    engine: String,
    voice: Option<String>,
    live: bool,
    attached_at: DateTime<Utc>,
    /// What it last reported (`speech.state`).
    state: Option<SpeechStateParams>,
    out: Arc<Outbox>,
    feed: tokio::task::AbortHandle,
}

/// Where everything sent to one speaker goes, one sender at a time. Only its feed holds the
/// channel's sender for good: once the feed ends, the speaker's stream ends.
struct Outbox {
    turn: tokio::sync::Mutex<()>,
    tx: mpsc::WeakSender<SpeechItem>,
}

impl Outbox {
    /// Sends `items` in order with nothing of another sender's between them, all or none:
    /// room for every one is reserved first, so a batch is never cut short; `false` when the
    /// speaker is gone or no room came within [`SEND_WAIT`] (then none was sent).
    async fn send_all(&self, items: &[SpeechItem]) -> bool {
        tokio::time::timeout(SEND_WAIT, async {
            let _turn = self.turn.lock().await;
            let Some(tx) = self.tx.upgrade() else {
                return false;
            };
            let Ok(permits) = tx.reserve_many(items.len()).await else {
                return false;
            };
            for (permit, item) in permits.zip(items) {
                permit.send(item.clone());
            }
            true
        })
        .await
        .unwrap_or(false)
    }
}

/// An attached speaker as its connection serves it ([`Hub::speech_attach`]): the ack, then its
/// items. Dropping it detaches the speaker and ends its feed.
pub struct SpeechAttachment {
    ack: SpeechAttachAck,
    rx: mpsc::Receiver<SpeechItem>,
    detach: Detach,
}

impl SpeechAttachment {
    /// The ack: the orchestrator thread followed and its head.
    pub fn ack(&self) -> &SpeechAttachAck {
        &self.ack
    }

    /// The speaker's key (`speech.state` names it).
    pub fn key(&self) -> u64 {
        self.detach.key
    }

    /// The next item; `None` once the feed ended (the hub stopped).
    pub async fn next(&mut self) -> Option<SpeechItem> {
        self.rx.recv().await
    }
}

/// Removes a speaker and ends its feed when dropped.
struct Detach {
    hub: Hub,
    key: u64,
}

impl Drop for Detach {
    fn drop(&mut self) {
        let gone = self.hub.speakers().by_key.remove(&self.key);
        if let Some(s) = gone {
            s.feed.abort();
        }
    }
}

/// What a speaker reads, from its attach.
struct Wants {
    replies: bool,
    commentary: bool,
    mine_only: bool,
    watermarks: BTreeMap<String, u64>,
}

impl Wants {
    /// Whether a text of `kind` is read.
    fn reads(&self, kind: MsgKind) -> bool {
        match kind {
            MsgKind::Reply => self.replies,
            MsgKind::Commentary => self.commentary,
            MsgKind::Message => true,
        }
    }
}

/// The backlog a feed reads after a watermark: the events up to `head`, those of before
/// `cutoff` counted (`count`, the newest `upto`) instead of sent.
struct Backlog {
    head: u64,
    cutoff: DateTime<Utc>,
    count: u64,
    upto: u64,
}

/// The thread a feed follows.
struct Follow {
    thread: String,
    sub: ThreadSubscription,
    /// The last seq handled (or skipped at the start): a subscription made again goes on
    /// after it.
    last: u64,
    backlog: Option<Backlog>,
    /// Whether each turn's texts are read.
    turns: BTreeMap<u64, bool>,
    /// A text held until its turn says what it is (a speaker reading one kind alone).
    held: Option<Arc<Event>>,
}

impl Follow {
    /// Follows `thread` after `watermark`, or from its head without one. With `catchup` (an
    /// attach's), the catch-up rule applies to what comes after the watermark, from now back
    /// `catchup`; without it (a new orchestrator), all of it is live.
    fn open(
        hub: &Hub,
        thread: &str,
        watermark: Option<u64>,
        catchup: Option<chrono::Duration>,
    ) -> Result<Follow, RpcError> {
        let mut sub = hub.subscribe_thread(thread, 0)?;
        let head = sub.head();
        let last = watermark.unwrap_or(head);
        sub.start_after(last);
        let backlog = match (watermark, catchup) {
            (Some(w), Some(catchup)) if w < head => Some(Backlog {
                head,
                cutoff: Utc::now() - catchup,
                count: 0,
                upto: 0,
            }),
            _ => None,
        };
        Ok(Follow {
            thread: sub.thread().to_string(),
            sub,
            last,
            backlog,
            turns: BTreeMap::new(),
            held: None,
        })
    }

    /// Why `ev` goes (live, or catch-up after the watermark); `None` when it is counted as
    /// missed instead.
    fn origin(&mut self, ev: &Event) -> Option<Origin> {
        match self.backlog.as_mut() {
            Some(b) if ev.seq <= b.head => {
                if ev.ts < b.cutoff {
                    b.count += 1;
                    b.upto = b.upto.max(ev.seq);
                    None
                } else {
                    Some(Origin::Catchup)
                }
            }
            _ => Some(Origin::Live),
        }
    }

    /// The `missed` item of what was counted since the last one, if anything was.
    fn take_missed(&mut self) -> Option<SpeechItem> {
        let b = self.backlog.as_mut().filter(|b| b.count > 0)?;
        let item = SpeechItem::Missed {
            thread: self.thread.clone(),
            count: b.count,
            upto: b.upto,
        };
        b.count = 0;
        Some(item)
    }

    /// Subscribes to the thread again, after the last event handled; what it remembers (the
    /// backlog, the turns, a text held) stays.
    fn resubscribe(&mut self, hub: &Hub) -> Result<(), RpcError> {
        let mut sub = hub.subscribe_thread(&self.thread, 0)?;
        sub.start_after(self.last);
        self.sub = sub;
        Ok(())
    }

    /// Remembers whether `turn`'s texts are read.
    fn remember(&mut self, turn: u64, readable: bool) {
        self.turns.insert(turn, readable);
        while self.turns.len() > TURNS_KEPT {
            self.turns.pop_first();
        }
    }
}

/// A speaker's feed task.
struct Feed {
    hub: Hub,
    wants: Wants,
    tx: mpsc::Sender<SpeechItem>,
    out: Arc<Outbox>,
}

/// What a feed woke for.
enum Wake {
    Control(anyhow::Result<Option<super::ControlItem>>),
    Event(anyhow::Result<Option<Arc<Event>>>),
    /// Time to subscribe again to the thread whose subscription failed.
    Retry,
}

/// The next event of `follow`; never, without one.
async fn next_event(follow: &mut Option<Follow>) -> anyhow::Result<Option<Arc<Event>>> {
    match follow {
        Some(f) => f.sub.next().await,
        None => std::future::pending().await,
    }
}

impl Feed {
    /// Runs until the hub stops or the speaker goes: `following` is the orchestrator thread
    /// as the attach read it, `follow` its subscription (none when there is no orchestrator
    /// or it could not be subscribed).
    async fn run(
        self,
        mut control: ControlSubscription,
        mut following: Option<String>,
        mut follow: Option<Follow>,
        seen: u64,
    ) {
        // The thread followed while its subscription is failed, subscribed again at
        // `retry_at`; the pause before the next try.
        let mut broken: Option<Follow> = None;
        let mut retry_at = tokio::time::Instant::now();
        let mut backoff = RETRY_FIRST;
        loop {
            let wake = tokio::select! {
                biased;
                c = control.next() => Wake::Control(c),
                e = next_event(&mut follow) => Wake::Event(e),
                () = tokio::time::sleep_until(retry_at), if broken.is_some() => Wake::Retry,
            };
            match wake {
                // A switch the orchestrator read at attach already reflects (it came between
                // the control subscription and that read) is no switch: the feed began there.
                Wake::Control(Ok(Some((gseq, _)))) if gseq <= seen => continue,
                Wake::Control(Ok(Some((_, ev)))) => {
                    // A switch or a reset is a new orchestrator thread, read from its start,
                    // one after the other in the log's order: each one left is read through
                    // what it said, even when several switches came before the feed got here.
                    // (One made before the control stream was subscribed is the one the feed
                    // started with.)
                    let Some(created) = orchestrator_created(&ev) else {
                        continue;
                    };
                    let now = Some(created);
                    if now == following {
                        continue;
                    }
                    following = now;
                    if let Some(old) = follow.take().or_else(|| broken.take()) {
                        if !self.leave(old).await {
                            return;
                        }
                    }
                    backoff = RETRY_FIRST;
                    let Some(thread) = following.clone() else {
                        continue;
                    };
                    let item = SpeechItem::Orchestrator {
                        thread: thread.clone(),
                    };
                    if !self.send(item).await {
                        return;
                    }
                    let from = self.wants.watermarks.get(&thread).copied().unwrap_or(0);
                    match Follow::open(&self.hub, &thread, Some(from), None) {
                        Ok(f) => follow = Some(f),
                        Err(e) => {
                            tracing::debug!(%thread, error = %e.message, "a speaker's feed cannot follow the orchestrator");
                        }
                    }
                }
                Wake::Control(Ok(None)) | Wake::Event(Ok(None)) => return,
                Wake::Control(Err(e)) => {
                    tracing::warn!(error = %format!("{e:#}"), "a speaker's feed lost the control stream");
                    return;
                }
                Wake::Event(Ok(Some(ev))) => {
                    let Some(f) = follow.as_mut() else {
                        continue;
                    };
                    backoff = RETRY_FIRST;
                    if !self.on_event(f, ev).await {
                        return;
                    }
                }
                // A store read failed: subscribed again after a pause, after the last event
                // handled, what the feed remembers of the thread kept.
                Wake::Event(Err(e)) => {
                    tracing::warn!(error = %format!("{e:#}"), retry_in = ?backoff, "a speaker's feed lost the orchestrator thread");
                    broken = follow.take();
                    retry_at = tokio::time::Instant::now() + backoff;
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
                Wake::Retry => {
                    let Some(mut f) = broken.take() else {
                        continue;
                    };
                    match f.resubscribe(&self.hub) {
                        Ok(()) => follow = Some(f),
                        // Gone (deleted): the next orchestrator change is followed.
                        Err(e) if e.code == codes::NOT_FOUND => {
                            tracing::debug!(thread = %f.thread, error = %e.message, "a speaker's feed cannot follow the orchestrator");
                        }
                        Err(e) => {
                            tracing::warn!(thread = %f.thread, error = %e.message, retry_in = ?backoff, "a speaker's feed cannot subscribe to the orchestrator again");
                            broken = Some(f);
                            retry_at = tokio::time::Instant::now() + backoff;
                            backoff = (backoff * 2).min(RETRY_MAX);
                        }
                    }
                }
            }
        }
    }

    /// Leaves `f`, the thread followed before a switch: the events it has logged up to now
    /// are handled as usual (all of them, through its head; [`LEAVE_WAIT`] bounds a store
    /// that stopped answering), then
    /// a text it still holds is sent as its turn's reply (the switch ends that turn for this
    /// feed), then what it counted as missed. `false` once the speaker is gone.
    async fn leave(&self, mut f: Follow) -> bool {
        // Through the thread's head as it is now: everything it said before the switch, however
        // much (a long turn's deltas and tools come before its last text).
        let head = match self.hub.subscribe_thread(&f.thread, 0) {
            Ok(s) => s.head(),
            Err(_) => f.last,
        };
        let deadline = tokio::time::Instant::now() + LEAVE_WAIT;
        let mut read = 0u64;
        while f.last < head {
            match tokio::time::timeout_at(deadline, f.sub.next()).await {
                Ok(Ok(Some(ev))) => {
                    if !self.on_event(&mut f, ev).await {
                        return false;
                    }
                    read += 1;
                    if read.is_multiple_of(256) {
                        tokio::task::yield_now().await;
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!(thread = %f.thread, error = %format!("{e:#}"), "a speaker's feed cannot read the rest of the orchestrator it left");
                    break;
                }
                Ok(Ok(None)) => break,
                Err(_) => {
                    tracing::debug!(thread = %f.thread, "a speaker's feed gave up reading the rest of the orchestrator it left");
                    break;
                }
            }
        }
        if let Some(h) = f.held.take() {
            if !self.deliver(&mut f, &h, MsgKind::Reply).await {
                return false;
            }
        }
        match f.take_missed() {
            Some(m) => self.send(m).await,
            None => true,
        }
    }

    /// Sends `item`; `false` once the speaker is gone.
    async fn send(&self, item: SpeechItem) -> bool {
        let _turn = self.out.turn.lock().await;
        self.tx.send(item).await.is_ok()
    }

    /// Handles one event of the thread followed; `false` once the speaker is gone.
    async fn on_event(&self, f: &mut Follow, ev: Arc<Event>) -> bool {
        f.last = f.last.max(ev.seq);
        let turn = ev.turn;
        let held_turn = f.held.as_ref().and_then(|h| h.turn);
        let ok = match &ev.body {
            EventBody::TurnStarted(_) => {
                // A text held of a turn before is that turn's last: its reply.
                let ok = match f.held.take() {
                    Some(h) if held_turn != turn => self.deliver(f, &h, MsgKind::Reply).await,
                    h => {
                        f.held = h;
                        true
                    }
                };
                if let Some(t) = turn {
                    let readable = self.start_readable(&ev);
                    f.remember(t, readable);
                }
                ok
            }
            EventBody::TurnEnded(_) | EventBody::TurnInterrupted(_) if held_turn == turn => {
                match f.held.take() {
                    Some(h) => self.deliver(f, &h, MsgKind::Reply).await,
                    None => true,
                }
            }
            // Taken back: nothing more of the turn is read, nor what it held.
            EventBody::TurnWithdrawn(_) => {
                if held_turn == turn {
                    f.held = None;
                }
                if let Some(t) = turn {
                    f.remember(t, false);
                }
                true
            }
            EventBody::AgentText(t) if t.is_final && !t.text.is_empty() => {
                self.on_final(f, ev.clone(), t).await
            }
            _ => true,
        };
        if !ok {
            return false;
        }
        // The backlog read: what it missed is said now, not with the next message.
        if f.backlog.as_ref().is_some_and(|b| ev.seq >= b.head) {
            if let Some(m) = f.take_missed() {
                return self.send(m).await;
            }
        }
        true
    }

    /// A final text: classified, then sent if the speaker reads its kind (see the module
    /// docs).
    async fn on_final(&self, f: &mut Follow, ev: Arc<Event>, t: &AgentText) -> bool {
        let Some(turn) = ev.turn else {
            return true;
        };
        let readable = match f.turns.get(&turn) {
            Some(r) => *r,
            None => {
                let r = turn_readable(&self.hub, &f.thread, turn, self.wants.mine_only);
                f.remember(turn, r);
                r
            }
        };
        if !readable {
            return true;
        }
        // A text held of this turn had one after it: commentary. Of a turn before: its reply.
        if let Some(h) = f.held.take() {
            let kind = if h.turn == ev.turn {
                MsgKind::Commentary
            } else {
                MsgKind::Reply
            };
            if !self.deliver(f, &h, kind).await {
                return false;
            }
        }
        match phase_kind(t) {
            Some(kind) => self.deliver(f, &ev, kind).await,
            None if self.wants.replies && self.wants.commentary => {
                self.deliver(f, &ev, MsgKind::Message).await
            }
            None => {
                f.held = Some(ev);
                true
            }
        }
    }

    /// Sends final text `ev` as `kind`, when the speaker reads that kind and it is not counted
    /// as missed (what was counted before it goes first).
    async fn deliver(&self, f: &mut Follow, ev: &Event, kind: MsgKind) -> bool {
        if !self.wants.reads(kind) {
            return true;
        }
        let Some(origin) = f.origin(ev) else {
            return true;
        };
        if let Some(m) = f.take_missed() {
            if !self.send(m).await {
                return false;
            }
        }
        match message(&self.hub, ev, kind, origin, None) {
            Some(m) => self.send(m).await,
            None => true,
        }
    }

    /// Whether the texts of the turn `ev` (its `turn.started`) starts are read.
    fn start_readable(&self, ev: &Event) -> bool {
        let withdrawn = ev
            .turn
            .is_none_or(|t| turn_withdrawn(&self.hub, &ev.thread, t));
        !withdrawn && start_readable(&self.hub, ev, self.wants.mine_only)
    }
}

/// The thread a control event makes the orchestrator, if it makes one: an orchestrator's
/// `thread.created` (`ext.twr.role`), as the hub's own fold takes it (`hub::orch`).
fn orchestrator_created(ev: &Event) -> Option<String> {
    let EventBody::ThreadCreated(p) = &ev.body else {
        return None;
    };
    let role = p
        .ext
        .as_ref()
        .and_then(|m| m.get("twr"))
        .and_then(|t| t.get("role"))
        .and_then(Value::as_str);
    (role == Some("orchestrator")).then(|| ev.thread.clone())
}

/// What Codex says a text is (`ext.codex.phase`), if it says.
fn phase_kind(t: &AgentText) -> Option<MsgKind> {
    let phase = t.ext.as_ref()?.get("codex")?.get("phase")?.as_str()?;
    match phase {
        "final_answer" => Some(MsgKind::Reply),
        "commentary" => Some(MsgKind::Commentary),
        _ => None,
    }
}

/// Whether the texts of the turn `start` (a `turn.started`) starts are read: a conversation
/// turn ([`speakable_turn`]), the user's when `mine_only`. Read from the blob store first
/// where it was capped; a part of `ext` that could not be is a marker for all it says
/// ([`speakable_turn`] fails closed), a long prompt is not (`ext` itself never moves).
fn start_readable(hub: &Hub, start: &Event, mine_only: bool) -> bool {
    let (start, _) = hydrated(hub, start);
    match &start.body {
        EventBody::TurnStarted(t) => speakable_turn(t) && (!mine_only || turn_is_users(t)),
        _ => false,
    }
}

/// Whether the texts of turn `turn` of `thread` are read: its `turn.started` (from `twr.db`,
/// then the archive), hydrated, says a conversation turn, and the user did not take it back.
/// One that cannot be read is not.
fn turn_readable(hub: &Hub, thread: &str, turn: u64, mine_only: bool) -> bool {
    let start = {
        let hot = store::turn_start(&hub.inner.reader(), thread, turn);
        match hot {
            Ok(Some(e)) => Ok(Some(e)),
            Ok(None) => match hub.inner.archive_reader().as_ref() {
                Some(a) => store::turn_start(a, thread, turn),
                None => Ok(None),
            },
            Err(e) => Err(e),
        }
    };
    let start = match start {
        Ok(Some(e)) => e,
        Ok(None) => return false,
        Err(e) => {
            tracing::warn!(%thread, turn, error = %format!("{e:#}"), "cannot read a turn's start; its texts are not read aloud");
            return false;
        }
    };
    start_readable(hub, &start, mine_only) && !turn_withdrawn(hub, thread, turn)
}

/// Whether the user took turn `turn` of `thread` back (`turn.withdrawn`); also when that
/// cannot be read.
fn turn_withdrawn(hub: &Hub, thread: &str, turn: u64) -> bool {
    store::last_of_kind(&hub.inner.reader(), thread, Some(turn), "turn.withdrawn")
        .map_or(true, |e| e.is_some())
}

/// `ev` with its capped fields restored (blobs from `twr.db`, then the archive), and whether
/// every one came back. One that cannot be read stays as it is.
fn hydrated(hub: &Hub, ev: &Event) -> (Event, bool) {
    if !ev.body.maybe_capped() {
        return (ev.clone(), true);
    }
    let full = (|| -> anyhow::Result<Option<(Event, bool)>> {
        let mut data = ev.body.data()?;
        if !has_capped(&data) {
            return Ok(None);
        }
        hydrate_with(&mut data, &mut |r: &str| hub.blob(r), true)?;
        let whole = !has_capped(&data);
        let body = EventBody::from_kind_data(ev.kind(), data)?;
        Ok(Some((Event { body, ..ev.clone() }, whole)))
    })();
    match full {
        Ok(Some(full)) => full,
        Ok(None) => (ev.clone(), true),
        Err(e) => {
            tracing::debug!(event = %ev.id, error = %format!("{e:#}"), "cannot hydrate a text to read aloud");
            (ev.clone(), false)
        }
    }
}

/// The `msg` of final text `ev`: its whole text (hydrated) up to [`TEXT_MAX`].
fn message(
    hub: &Hub,
    ev: &Event,
    kind: MsgKind,
    origin: Origin,
    batch: Option<u64>,
) -> Option<SpeechItem> {
    let (full, whole) = hydrated(hub, ev);
    let EventBody::AgentText(t) = &full.body else {
        return None;
    };
    let (text, cut) = cap_text(&t.text, TEXT_MAX);
    Some(SpeechItem::Msg {
        thread: ev.thread.clone(),
        seq: ev.seq,
        turn: ev.turn,
        ts: ev.ts.to_rfc3339_opts(SecondsFormat::Millis, true),
        kind,
        text,
        truncated: cut || !whole,
        origin,
        batch,
    })
}

/// Refuses the orchestrator.
fn users_only(caller: &Caller) -> Result<(), RpcError> {
    if caller.is_orchestrator() {
        return Err(rpc(codes::UNAUTHORIZED, "reading aloud is the user's"));
    }
    Ok(())
}

impl Hub {
    fn speakers(&self) -> std::sync::MutexGuard<'_, Speakers> {
        self.inner
            .speech
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `speech.attach` (users only): attaches a speaker and starts its feed (see the module
    /// docs). `conflict` while a speaker of the same device is attached.
    pub fn speech_attach(
        &self,
        p: SpeechAttachParams,
        caller: &Caller,
    ) -> Result<SpeechAttachment, RpcError> {
        users_only(caller)?;
        if let Some(e) = p.error() {
            return Err(rpc(codes::BAD_REQUEST, e));
        }
        let conflict = || {
            rpc(
                codes::CONFLICT,
                format!("a speaker of device {} is attached", p.device),
            )
        };
        if self
            .speakers()
            .by_key
            .values()
            .any(|s| s.device == p.device)
        {
            return Err(conflict());
        }
        let catchup = chrono::Duration::seconds(p.catchup_secs.min(i64::MAX as u64) as i64);
        // The control stream first, then the orchestrator: a switch after the read comes as a
        // control item.
        let since = self.inner.core().gseq;
        let control = self.subscribe_control(since);
        // The orchestrator and the control position it is as of, read together: control items
        // up to that position are already in it.
        let (following, seen) = {
            let core = self.inner.core();
            (core.orch.thread.clone(), core.gseq)
        };
        let follow = following.as_deref().and_then(|t| {
            Follow::open(self, t, p.watermarks.get(t).copied(), Some(catchup))
                .inspect_err(|e| {
                    tracing::debug!(thread = %t, error = %e.message, "a speaker's feed cannot follow the orchestrator");
                })
                .ok()
        });
        let ack = SpeechAttachAck {
            orchestrator: follow.as_ref().map(|f| f.thread.clone()),
            head: follow.as_ref().map_or(0, |f| f.sub.head()),
        };
        let wants = Wants {
            replies: p.read.contains(&ReadKind::Replies),
            commentary: p.read.contains(&ReadKind::Commentary),
            mine_only: p.turn_from == TurnsRead::Mine,
            watermarks: p.watermarks,
        };
        let (tx, rx) = mpsc::channel(QUEUE);
        let out = Arc::new(Outbox {
            turn: tokio::sync::Mutex::new(()),
            tx: tx.downgrade(),
        });
        let key = {
            let mut speakers = self.speakers();
            if speakers.by_key.values().any(|s| s.device == p.device) {
                return Err(conflict());
            }
            let key = speakers.next_key;
            speakers.next_key += 1;
            let feed = Feed {
                hub: self.clone(),
                wants,
                tx,
                out: Arc::clone(&out),
            };
            let task = tokio::spawn(feed.run(control, following, follow, seen));
            speakers.by_key.insert(
                key,
                Speaker {
                    device: p.device,
                    name: p.name,
                    engine: p.engine,
                    voice: p.voice,
                    live: p.live,
                    attached_at: Utc::now(),
                    state: None,
                    out,
                    feed: task.abort_handle(),
                },
            );
            key
        };
        Ok(SpeechAttachment {
            ack,
            rx,
            detach: Detach {
                hub: self.clone(),
                key,
            },
        })
    }

    /// `speech.state` from the connection of speaker `key`: what it plays, for
    /// `speech.status`.
    pub fn speech_state(&self, key: u64, mut p: SpeechStateParams) -> Result<Value, RpcError> {
        if let Some(v) = &p.voice {
            if let Some(e) = name_error("voice", v, 128) {
                return Err(rpc(codes::BAD_REQUEST, e));
            }
        }
        if let Some(playing) = &p.playing {
            if playing.thread.len() > 64 {
                return Err(rpc(codes::BAD_REQUEST, "playing.thread: no thread id"));
            }
        }
        p.error = p
            .error
            .map(|e| clean(&e).chars().take(STATE_ERROR_MAX).collect());
        let mut speakers = self.speakers();
        let Some(s) = speakers.by_key.get_mut(&key) else {
            return Err(rpc(
                codes::BAD_REQUEST,
                "the speaker of this connection is no longer attached",
            ));
        };
        s.state = Some(p);
        Ok(json!({ "ok": true }))
    }

    /// `speech.status` (users only): the speakers attached, in the order they attached.
    pub fn speech_status(&self, caller: &Caller) -> Result<SpeechStatus, RpcError> {
        users_only(caller)?;
        let speakers = self.speakers();
        Ok(SpeechStatus {
            speakers: speakers
                .by_key
                .values()
                .map(|s| SpeakerStatus {
                    device: s.device.clone(),
                    name: s.name.clone(),
                    engine: s.engine.clone(),
                    since: s.attached_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                    state: s.state.clone().unwrap_or_else(|| SpeechStateParams {
                        live: s.live,
                        voice: s.voice.clone(),
                        ..Default::default()
                    }),
                })
                .collect(),
        })
    }

    /// The speakers `speaker` names (by name or device; every one attached without it), and
    /// a new batch id. `not_found` when it names none.
    fn speech_targets(&self, speaker: Option<&str>) -> Result<(Vec<Arc<Outbox>>, u64), RpcError> {
        let mut speakers = self.speakers();
        let targets: Vec<Arc<Outbox>> = speakers
            .by_key
            .values()
            .filter(|s| speaker.is_none_or(|n| s.name == n || s.device == n))
            .map(|s| Arc::clone(&s.out))
            .collect();
        if let (Some(n), true) = (speaker, targets.is_empty()) {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("no speaker named {n} is attached"),
            ));
        }
        speakers.next_batch += 1;
        Ok((targets, speakers.next_batch))
    }

    /// `speech.request` (users only): reads messages to the speakers now (see the module
    /// docs).
    pub async fn speech_request(
        &self,
        p: &SpeechRequestParams,
        caller: &Caller,
    ) -> Result<SpeechRequestResult, RpcError> {
        users_only(caller)?;
        let thread = match &p.thread {
            Some(q) => self.resolve(q)?,
            None => self
                .orchestrator_thread()
                .ok_or_else(|| rpc(codes::NOT_FOUND, "there is no orchestrator yet"))?,
        };
        let finals = match (p.last, p.seq) {
            (Some(n), None) if (1..=REQUEST_MAX).contains(&n) => {
                self.last_messages(&thread, n as usize)?
            }
            (Some(_), None) => {
                return Err(rpc(codes::BAD_REQUEST, format!("last: 1 to {REQUEST_MAX}")))
            }
            (None, Some(seq)) => vec![self.message_at(&thread, seq)?],
            _ => return Err(rpc(codes::BAD_REQUEST, "give exactly one of last and seq")),
        };
        let (targets, batch) = self.speech_targets(p.speaker.as_deref())?;
        let msgs: Vec<SpeechItem> = finals
            .iter()
            .filter_map(|ev| {
                let kind = match &ev.body {
                    EventBody::AgentText(t) => phase_kind(t).unwrap_or(MsgKind::Message),
                    _ => MsgKind::Message,
                };
                message(self, ev, kind, Origin::Request, Some(batch))
            })
            .collect();
        let count = msgs.len() as u32;
        let mut items = vec![SpeechItem::Ctl {
            op: CtlOp::Replace,
            value: Some(json!({ "batch": batch, "count": count })),
        }];
        items.extend(msgs);
        let mut speakers = 0;
        for out in targets {
            if out.send_all(&items).await {
                speakers += 1;
            }
        }
        Ok(SpeechRequestResult {
            count,
            speakers,
            thread,
        })
    }

    /// `speech.control` (users only): sends a control to the speakers asked.
    pub async fn speech_control(
        &self,
        p: &SpeechControlParams,
        caller: &Caller,
    ) -> Result<SpeechControlResult, RpcError> {
        users_only(caller)?;
        if let Some(e) = p.error() {
            return Err(rpc(codes::BAD_REQUEST, e));
        }
        let (targets, _) = self.speech_targets(p.speaker.as_deref())?;
        let item = [SpeechItem::Ctl {
            op: p.op,
            value: p.value.clone(),
        }];
        let mut speakers = 0;
        for out in targets {
            if out.send_all(&item).await {
                speakers += 1;
            }
        }
        Ok(SpeechControlResult { speakers })
    }

    /// A page of the final texts of `thread` before `before` (all without it), newest first:
    /// `twr.db`'s, then the archive's, merged by seq.
    fn finals_page(
        &self,
        thread: &str,
        before: Option<u64>,
        limit: usize,
    ) -> anyhow::Result<Vec<Event>> {
        let mut page = store::speakable_finals(&self.inner.reader(), thread, before, limit)?;
        let archived = match self.inner.archive_reader().as_ref() {
            Some(a) => store::speakable_finals(a, thread, before, limit)?,
            None => Vec::new(),
        };
        if !archived.is_empty() {
            let have: std::collections::HashSet<u64> = page.iter().map(|e| e.seq).collect();
            page.extend(archived.into_iter().filter(|e| !have.contains(&e.seq)));
            page.sort_by_key(|e| std::cmp::Reverse(e.seq));
            page.truncate(limit);
        }
        Ok(page)
    }

    /// The last `n` messages of `thread` read aloud, oldest first: its final texts of
    /// conversation turns, read backwards a page at a time, [`PAGES`] pages at most.
    fn last_messages(&self, thread: &str, n: usize) -> Result<Vec<Event>, RpcError> {
        let mut out = Vec::new();
        let mut turns: HashMap<u64, bool> = HashMap::new();
        let mut before = None;
        'pages: for _ in 0..PAGES {
            let page = self.finals_page(thread, before, PAGE).map_err(internal)?;
            let Some(last) = page.last() else {
                break;
            };
            before = Some(last.seq);
            let full = page.len() == PAGE;
            for ev in page {
                let (Some(turn), EventBody::AgentText(t)) = (ev.turn, &ev.body) else {
                    continue;
                };
                if t.text.is_empty() {
                    continue;
                }
                let readable = *turns
                    .entry(turn)
                    .or_insert_with(|| turn_readable(self, thread, turn, false));
                if readable {
                    out.push(ev);
                    if out.len() == n {
                        break 'pages;
                    }
                }
            }
            if !full {
                break;
            }
        }
        out.reverse();
        Ok(out)
    }

    /// The event `seq` of `thread`, from `twr.db`, then the archive.
    fn event_at(&self, thread: &str, seq: u64) -> anyhow::Result<Option<Event>> {
        let hot = store::event_at(&self.inner.reader(), thread, seq)?;
        match hot {
            Some(e) => Ok(Some(e)),
            None => match self.inner.archive_reader().as_ref() {
                Some(a) => store::event_at(a, thread, seq),
                None => Ok(None),
            },
        }
    }

    /// The message of `thread` `seq` names: its final `agent.text`, or one of its deltas (the
    /// turn's next final text from there: the TUI may know only the first delta's seq), of a
    /// turn read aloud.
    fn message_at(&self, thread: &str, seq: u64) -> Result<Event, RpcError> {
        let bad = |m: String| rpc(codes::BAD_REQUEST, m);
        let ev = self
            .event_at(thread, seq)
            .map_err(internal)?
            .ok_or_else(|| rpc(codes::NOT_FOUND, format!("{thread} has no event {seq}")))?;
        let EventBody::AgentText(t) = &ev.body else {
            return Err(bad(format!("event {seq} of {thread} is no message")));
        };
        let Some(turn) = ev.turn else {
            return Err(bad(format!("event {seq} of {thread} is no message")));
        };
        let ev = if t.is_final {
            ev
        } else {
            let hot = store::next_final_text(&self.inner.reader(), thread, turn, seq)
                .map_err(internal)?;
            let found = match hot {
                Some(e) => Some(e),
                None => match self.inner.archive_reader().as_ref() {
                    Some(a) => store::next_final_text(a, thread, turn, seq).map_err(internal)?,
                    None => None,
                },
            };
            found.ok_or_else(|| bad(format!("the message at {seq} is still being written")))?
        };
        if !turn_readable(self, thread, turn, false) {
            return Err(bad(format!(
                "the message at {seq} is not read aloud (a handoff, a compaction or a turn \
                 taken back)"
            )));
        }
        Ok(ev)
    }
}
