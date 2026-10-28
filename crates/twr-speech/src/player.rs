//! The player: a queue of messages and what is being said, as a pure state machine
//! ([`Machine`]), and the driver that makes an [`Engine`] say what the machine says is current
//! ([`Player`]).
//!
//! - **Order.** Messages are said in the order they came. A `/read N` batch ([`Machine::replace`])
//!   stops what is said, empties the queue and goes first; live messages that come after it
//!   queue behind it. Only the latest batch counts: request messages of any other are dropped,
//!   and a `replace` of an older batch than one already seen from that hub (a hub numbers its
//!   batches in order; two requests may reach the speaker the other way round) is ignored. A
//!   batch none of whose messages came for [`BATCH_IDLE_TICKS`] ticks ([`Machine::tick`]) ends
//!   with what came: a message the speaker couldn't read (a newer hub's) never holds the rest.
//! - **Live.** A live (or catch-up) message of a workspace whose live reading is off is not
//!   queued; it counts as handled.
//! - **Never twice.** A live or catch-up message is played at most once per process: the
//!   machine remembers the last [`SEEN_MAX`] `(thread, seq)` it got, and drops one it saw.
//!   Request messages are exempt (you asked).
//! - **Watermarks** ([`Effect::Watermark`]): a live or catch-up message's seq is handled once
//!   it starts playing, or once it is dropped (live off, stop, replace, an engine failure).
//!   So a restart mid-message never repeats it.
//! - **Prefaces.** `Reading N messages` before a batch of several; the workspace's name when
//!   several workspaces read live; `long message, about N minutes` for one over two minutes;
//!   `message cut short; the rest is on screen` after a truncated one.

use std::collections::{BTreeMap, HashSet, VecDeque};

use twr_core::speech::{estimate_secs, Origin, Playing, SpeechStateParams};

use crate::engine::{Engine, Prefs, SpeakFuture};

/// How many `(thread, seq)` the machine remembers for de-duplication.
pub const SEEN_MAX: usize = 4096;

/// A message estimated longer than this many seconds gets the `long message` preface.
pub const LONG_SECS: u64 = 120;

/// How many [`Machine::tick`]s a batch still coming may go without a message of it before it
/// ends with what came (the speaker ticks every second).
pub const BATCH_IDLE_TICKS: u32 = 10;

/// A message to say, as it reaches the player (its text already in utterances).
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    /// The workspace (of the link) it came from.
    pub ws: String,
    /// Its thread.
    pub thread: String,
    /// The seq of its final text.
    pub seq: u64,
    /// Its utterances (`twr_core::speech::utterances`).
    pub chunks: Vec<String>,
    /// Why it was sent.
    pub origin: Origin,
    /// Its `/read N` batch (request messages).
    pub batch: Option<u64>,
    /// Not all of it came.
    pub truncated: bool,
}

/// What the machine asks of the speaker around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Everything of `thread` up to `seq` was handled: persist it (as a maximum).
    Watermark {
        /// The thread.
        thread: String,
        /// Its seq.
        seq: u64,
    },
    /// As [`Effect::Watermark`], for a message that starts now: persisted once the engine
    /// stopped what it said before (`spd-say -C` may take a moment) and is saying it, so a
    /// speaker ended in between doesn't count it as read.
    Started {
        /// The thread.
        thread: String,
        /// Its seq.
        seq: u64,
    },
}

/// An entry of the queue.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    /// A message.
    Msg(Msg),
    /// Something to say that is no message (`N earlier messages came while I was
    /// disconnected`).
    Notice {
        /// The workspace it is about.
        ws: String,
        /// What to say.
        text: String,
    },
}

impl Entry {
    fn ws(&self) -> &str {
        match self {
            Entry::Msg(m) => &m.ws,
            Entry::Notice { ws, .. } => ws,
        }
    }
}

/// Which chunk of which started entry is current: what [`Machine::chunk_done`] reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The started entry (unique per start).
    pub play: u64,
    /// The utterance, from 0.
    pub chunk: usize,
}

/// The entry being said.
#[derive(Debug, Clone)]
struct Current {
    play: u64,
    entry: Entry,
    utterances: Vec<String>,
    chunk: usize,
    /// Its place in the batch, if it is a batch message: (n from 1, of).
    batch: Option<(u32, u32)>,
}

/// The `/read N` batch being read.
#[derive(Debug, Clone)]
struct Batch {
    ws: String,
    id: u64,
    count: u32,
    received: u32,
    started: u32,
    /// Ticks since the last of its messages came (or its `replace`).
    idle: u32,
}

/// The player's state (see the module documentation). Pure: no engine, no time.
#[derive(Debug)]
pub struct Machine {
    queue: VecDeque<Entry>,
    /// How many entries at the queue's front are the batch's.
    batch_queued: usize,
    batch: Option<Batch>,
    current: Option<Current>,
    paused: bool,
    live: BTreeMap<String, bool>,
    /// The newest batch id each workspace's hub sent (its ids only grow while it is attached).
    latest_batch: BTreeMap<String, u64>,
    seen: HashSet<(String, u64)>,
    /// Per thread, places handled that its watermark could not move to yet (a message waits
    /// before them): reached by the next mark that may ([`Machine::mark`]).
    deferred: BTreeMap<String, std::collections::BTreeSet<u64>>,
    seen_order: VecDeque<(String, u64)>,
    prefs: Prefs,
    next_play: u64,
    error: Option<String>,
    effects: Vec<Effect>,
}

impl Machine {
    /// An idle player speaking with `prefs`; live reading off everywhere.
    pub fn new(prefs: Prefs) -> Machine {
        Machine {
            queue: VecDeque::new(),
            batch_queued: 0,
            batch: None,
            current: None,
            paused: false,
            live: BTreeMap::new(),
            latest_batch: BTreeMap::new(),
            seen: HashSet::new(),
            deferred: BTreeMap::new(),
            seen_order: VecDeque::new(),
            prefs,
            next_play: 1,
            error: None,
            effects: Vec::new(),
        }
    }

    /// The effects produced since the last call.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// The prefs chunks are said with.
    pub fn prefs(&self) -> &Prefs {
        &self.prefs
    }

    /// Paused.
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// The last engine error, until a chunk is said again.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether live reading is on for `ws`.
    pub fn live(&self, ws: &str) -> bool {
        self.live.get(ws).copied().unwrap_or(false)
    }

    /// The chunk that should be audible now (if not paused), and its text.
    pub fn current(&self) -> Option<(Key, &str)> {
        let c = self.current.as_ref()?;
        Some((
            Key {
                play: c.play,
                chunk: c.chunk,
            },
            c.utterances.get(c.chunk)?.as_str(),
        ))
    }

    /// How many entries wait (not counting the current one).
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Whether anything is being said or waits.
    pub fn busy(&self) -> bool {
        self.current.is_some() || !self.queue.is_empty()
    }

    /// Remembers `(thread, seq)`; whether it was new.
    fn first_sight(&mut self, thread: &str, seq: u64) -> bool {
        let k = (thread.to_string(), seq);
        if self.seen.contains(&k) {
            return false;
        }
        // What waits or plays counts as seen however long ago it came: the bounded memory
        // above may have let go of it while more than [`SEEN_MAX`] waited (paused).
        let here = |e: &Entry| matches!(e, Entry::Msg(m) if m.thread == thread && m.seq == seq && m.origin != Origin::Request);
        if self.queue.iter().any(here) || self.current.as_ref().is_some_and(|c| here(&c.entry)) {
            return false;
        }
        self.remember(thread, seq);
        true
    }

    /// Notes `(thread, seq)` as seen (the memory keeps the last [`SEEN_MAX`]).
    fn remember(&mut self, thread: &str, seq: u64) {
        let k = (thread.to_string(), seq);
        if !self.seen.insert(k.clone()) {
            return;
        }
        if self.seen_order.len() >= SEEN_MAX {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
        self.seen_order.push_back(k);
    }

    /// `msg` was handled without playing (dropped): its watermark moves, for a live one.
    fn handled(&mut self, entry: &Entry) {
        if let Entry::Msg(m) = entry {
            if m.origin != Origin::Request {
                self.mark(&m.thread, m.seq);
            }
        }
    }

    /// `thread` was handled up to `seq`: its watermark moves there, but never past a live or
    /// catch-up message of that thread still waiting in the queue (only to just before the
    /// first of them), so a restart never skips one. A place it can't move to yet is kept
    /// ([`Machine::deferred`]) and reached once what waits before it is handled, so a restart
    /// doesn't say again what was said (a message asked for, then come live).
    fn mark(&mut self, thread: &str, seq: u64) {
        self.mark_as(thread, seq, false);
    }

    /// [`Machine::mark`], as a start ([`Effect::Started`]) when `started`.
    fn mark_as(&mut self, thread: &str, seq: u64, started: bool) {
        let waiting = self
            .queue
            .iter()
            .filter_map(|e| match e {
                Entry::Msg(m) if m.thread == thread && m.origin != Origin::Request => Some(m.seq),
                _ => None,
            })
            .min();
        let deferred = self.deferred.entry(thread.to_string()).or_default();
        deferred.insert(seq);
        let top = deferred.iter().next_back().copied().unwrap_or(seq);
        let to = match waiting {
            Some(w) => top.min(w.saturating_sub(1)),
            None => top,
        };
        deferred.retain(|&d| d > to);
        if deferred.is_empty() {
            self.deferred.remove(thread);
        }
        let thread = thread.to_string();
        self.effects.push(if started {
            Effect::Started { thread, seq: to }
        } else {
            Effect::Watermark { thread, seq: to }
        });
    }

    /// A message came.
    pub fn push(&mut self, msg: Msg) {
        if msg.origin == Origin::Request {
            let ours = self
                .batch
                .as_mut()
                .filter(|b| Some(b.id) == msg.batch && b.ws == msg.ws && b.received < b.count);
            let Some(b) = ours else {
                tracing::debug!(seq = msg.seq, "dropping a message of another batch");
                return;
            };
            b.idle = 0;
            if msg.chunks.is_empty() {
                // Nothing to say: the batch is one shorter (and may be all here now, which
                // lets what waits behind it go on).
                b.count -= 1;
                self.advance();
                return;
            }
            b.received += 1;
            // Asked for, so read whatever was read before; but known from now on, so the same
            // message coming live after it (the feed behind the request) isn't read again.
            self.remember(&msg.thread, msg.seq);
            self.queue.insert(self.batch_queued, Entry::Msg(msg));
            self.batch_queued += 1;
            self.advance();
            return;
        }
        if !self.first_sight(&msg.thread, msg.seq) {
            // Seen: said or let go before (or asked for), or waiting. Handled either way: its
            // watermark moves (never past one still waiting), so a restart doesn't say it.
            self.mark(&msg.thread, msg.seq);
            return;
        }
        let entry = Entry::Msg(msg);
        let Entry::Msg(m) = &entry else {
            unreachable!()
        };
        if !self.live(&m.ws) || m.chunks.is_empty() {
            self.handled(&entry);
            return;
        }
        self.queue.push_back(entry);
        self.advance();
    }

    /// `ws`'s hub asked for batch `id` of `count` messages (`/read N`): stop, empty the queue,
    /// and read those next. Ignored when that hub already sent a batch numbered `id` or
    /// higher: the older request reached this speaker last, and its messages are dropped.
    pub fn replace(&mut self, ws: &str, id: u64, count: u32) {
        if self
            .latest_batch
            .get(ws)
            .is_some_and(|&latest| id <= latest)
        {
            tracing::debug!(id, "ignoring the replace of an older batch");
            return;
        }
        self.latest_batch.insert(ws.to_string(), id);
        self.clear();
        // Asked for: read now, paused or not.
        self.paused = false;
        self.batch = Some(Batch {
            ws: ws.to_string(),
            id,
            count,
            received: 0,
            started: 0,
            idle: 0,
        });
    }

    /// A second went by (the speaker ticks every second; the machine keeps no clock): a batch
    /// still coming whose last message (or `replace`) came [`BATCH_IDLE_TICKS`] ticks ago ends
    /// with what came, so what waits behind it goes on. A hub sends a batch's messages
    /// together; the ones missing are ones this speaker could not read (a newer hub's), which
    /// would otherwise hold live reading forever.
    pub fn tick(&mut self) {
        let Some(b) = self.batch.as_mut().filter(|b| b.received < b.count) else {
            return;
        };
        b.idle += 1;
        if b.idle >= BATCH_IDLE_TICKS {
            tracing::debug!(
                id = b.id,
                received = b.received,
                count = b.count,
                "a batch's other messages never came"
            );
            b.count = b.received;
            self.advance();
        }
    }

    /// `ws`'s hub went away: a batch of it still coming never will. What came of it is read;
    /// what waited behind it goes on.
    ///
    /// Its live and catch-up messages that haven't started are let go too, without moving
    /// their watermark, and forgotten: they are sent again to whichever speaker of this device
    /// attaches next (this one, or the one that stood by), and said once, by that one. Kept,
    /// both could say them.
    ///
    /// Its live switch goes too (an attach sets it again), so a hub gone no longer counts
    /// among the workspaces reading live; and the batch ids it sent, which a restarted hub
    /// numbers from 1 again.
    pub fn detached(&mut self, ws: &str) {
        if let Some(b) = self.batch.as_mut().filter(|b| b.ws == ws) {
            b.count = b.received;
        }
        self.live.remove(ws);
        self.latest_batch.remove(ws);
        let mut kept = VecDeque::new();
        let mut kept_batch = 0;
        let mut forget = Vec::new();
        for (i, e) in std::mem::take(&mut self.queue).into_iter().enumerate() {
            match &e {
                Entry::Msg(m) if m.ws == ws && m.origin != Origin::Request => {
                    forget.push((m.thread.clone(), m.seq));
                }
                _ => {
                    if i < self.batch_queued {
                        kept_batch += 1;
                    }
                    kept.push_back(e);
                }
            }
        }
        self.queue = kept;
        self.batch_queued = kept_batch;
        for (thread, seq) in forget {
            // A place held back behind a message let go would let the watermark pass it when
            // the next one is handled: the hub sends them again from the watermark, and they
            // are handled again then.
            if let Some(d) = self.deferred.get_mut(&thread) {
                d.retain(|&p| p < seq);
                if d.is_empty() {
                    self.deferred.remove(&thread);
                }
            }
            let k = (thread, seq);
            self.seen.remove(&k);
            self.seen_order.retain(|o| *o != k);
        }
        self.advance();
    }

    /// Stop now and empty the queue (a batch still coming is dropped too).
    pub fn stop(&mut self) {
        self.clear();
        // Nothing left to resume: what comes next plays.
        self.paused = false;
        self.batch = None;
    }

    /// Ends the current entry and drops the queue.
    fn clear(&mut self) {
        self.current = None;
        let dropped: Vec<Entry> = self.queue.drain(..).collect();
        for e in &dropped {
            self.handled(e);
        }
        self.batch_queued = 0;
    }

    /// Pause.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Resume.
    pub fn resume(&mut self) {
        self.paused = false;
        self.advance();
    }

    /// Pause, or resume when paused (the `p` key).
    pub fn toggle_pause(&mut self) {
        if self.paused {
            self.resume()
        } else {
            self.pause()
        }
    }

    /// End the current entry and go on with the next.
    pub fn skip(&mut self) {
        if self.current.take().is_some() {
            self.advance();
        }
    }

    /// Live reading on or off for `ws`. Off drops `ws`'s live messages, the current one too.
    pub fn set_live(&mut self, ws: &str, on: bool) {
        self.live.insert(ws.to_string(), on);
        if on {
            return;
        }
        let is_live = |e: &Entry| match e {
            Entry::Msg(m) => m.ws == ws && m.origin != Origin::Request,
            Entry::Notice { ws: w, .. } => w == ws,
        };
        if self.current.as_ref().is_some_and(|c| is_live(&c.entry)) {
            self.current = None;
        }
        let mut kept = VecDeque::new();
        let mut kept_batch = 0;
        for (i, e) in std::mem::take(&mut self.queue).into_iter().enumerate() {
            if is_live(&e) {
                self.handled(&e);
            } else {
                if i < self.batch_queued {
                    kept_batch += 1;
                }
                kept.push_back(e);
            }
        }
        self.queue = kept;
        self.batch_queued = kept_batch;
        self.advance();
    }

    /// `count` messages of `thread` (up to `upto`) were said while the speaker was away, too
    /// long ago to send: handled; said so when `ws` reads live.
    pub fn missed(&mut self, ws: &str, thread: &str, count: u64, upto: u64) {
        self.mark(thread, upto);
        if count == 0 || !self.live(ws) {
            return;
        }
        // No `/read N` to offer: that reads the last N, which the catch-up messages after
        // these are among.
        let text = if count == 1 {
            "1 earlier message came while I was disconnected; it's on screen".to_string()
        } else {
            format!("{count} earlier messages came while I was disconnected; they're on screen")
        };
        self.queue.push_back(Entry::Notice {
            ws: ws.to_string(),
            text,
        });
        self.advance();
    }

    /// Use the voice `voice` from the next chunk on.
    pub fn set_voice(&mut self, voice: Option<String>) {
        self.prefs.voice = voice;
    }

    /// Speak at `rate` words per minute from the next chunk on.
    pub fn set_rate(&mut self, rate: u32) {
        self.prefs.rate = rate;
    }

    /// The chunk `key` ended: said (`Ok`), or the engine failed (`Err`: an error state, the
    /// queue dropped). A key that is no longer current is ignored.
    pub fn chunk_done(&mut self, key: Key, result: Result<(), String>) {
        if self.current().map(|(k, _)| k) != Some(key) {
            return;
        }
        match result {
            Ok(()) => {
                self.error = None;
                let c = self.current.as_mut().expect("current");
                c.chunk += 1;
                if c.chunk >= c.utterances.len() {
                    self.current = None;
                    self.advance();
                }
            }
            Err(e) => {
                self.error = Some(e);
                self.stop();
            }
        }
    }

    /// Starts the next entry when nothing is current and the player isn't paused.
    fn advance(&mut self) {
        if self.current.is_some() || self.paused {
            return;
        }
        // A batch still coming goes first: nothing else starts until its next message is here
        // (or the batch ended: all came, stopped, replaced, or its hub went away).
        if self.batch_queued == 0 && self.batch.as_ref().is_some_and(|b| b.received < b.count) {
            return;
        }
        let Some(entry) = self.queue.pop_front() else {
            return;
        };
        let in_batch = self.batch_queued > 0;
        if in_batch {
            self.batch_queued -= 1;
        }
        let several_live = self.live.values().filter(|on| **on).count() > 1;
        let mut utterances = Vec::new();
        let mut batch = None;
        if in_batch {
            if let Some(b) = &mut self.batch {
                if b.started == 0 && b.count > 1 {
                    utterances.push(format!("Reading {} messages.", b.count));
                }
                b.started += 1;
                batch = Some((b.started, b.count));
            }
        }
        if several_live {
            utterances.push(format!("{}:", entry.ws()));
        }
        match &entry {
            Entry::Msg(m) => {
                let secs = estimate_secs(&m.chunks, self.prefs.rate);
                if secs > LONG_SECS {
                    utterances.push(format!(
                        "long message, about {} minutes.",
                        secs.div_ceil(60)
                    ));
                }
                utterances.extend(m.chunks.iter().cloned());
                if m.truncated {
                    utterances.push("message cut short; the rest is on screen.".into());
                }
                if m.origin != Origin::Request {
                    // Handled from its start: a restart mid-message never repeats it.
                    let (thread, seq) = (m.thread.clone(), m.seq);
                    self.mark_as(&thread, seq, true);
                }
            }
            Entry::Notice { text, .. } => utterances.push(text.clone()),
        }
        let play = self.next_play;
        self.next_play += 1;
        self.current = Some(Current {
            play,
            entry,
            utterances,
            chunk: 0,
            batch,
        });
    }

    /// What the speaker reports to `ws`'s hub (`speech.state`): what it plays only when that is
    /// `ws`'s, and `ws`'s queue.
    pub fn state_for(&self, ws: &str) -> SpeechStateParams {
        let current = self.current.as_ref().filter(|c| c.entry.ws() == ws);
        SpeechStateParams {
            playing: current.and_then(|c| match &c.entry {
                Entry::Msg(m) => Some(Playing {
                    thread: m.thread.clone(),
                    seq: m.seq,
                    chunk: c.chunk as u32 + 1,
                    of: c.utterances.len() as u32,
                }),
                Entry::Notice { .. } => None,
            }),
            // An announcement being said (`N messages came while…`) is no message to mark, but
            // something is being said: it counts as waiting, so `Ctrl-R` stops it.
            queued: self.queue.iter().filter(|e| e.ws() == ws).count() as u32
                + u32::from(current.is_some_and(|c| matches!(c.entry, Entry::Notice { .. }))),
            batch: current.and_then(|c| c.batch),
            paused: self.paused,
            live: self.live(ws),
            voice: self.prefs.voice.clone(),
            rate: self.prefs.rate,
            error: self.error.clone(),
        }
    }
}

/// The chunk being said.
struct Active {
    key: Key,
    future: SpeakFuture,
    /// Paused in place by the engine.
    native_paused: bool,
}

/// A [`Machine`] and an [`Engine`]: the engine says what the machine says is current.
/// After each change to the machine, call [`Player::sync`]; [`Player::next_done`] waits for
/// the chunk being said to end.
pub struct Player {
    /// The state.
    pub machine: Machine,
    engine: Box<dyn Engine>,
    active: Option<Active>,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("machine", &self.machine)
            .field("engine", &self.engine.name())
            .finish_non_exhaustive()
    }
}

impl Player {
    /// Ends what is being said, as quitting does: the chunk's future dropped, then whatever
    /// the engine still has silenced ([`Engine::after_stop`]: `spd-say -C`).
    pub async fn silence(&mut self) {
        if self.active.take().is_some() {
            self.engine.after_stop().await;
        }
    }

    /// A player of `engine` speaking with `prefs`.
    pub fn new(engine: Box<dyn Engine>, prefs: Prefs) -> Player {
        Player {
            machine: Machine::new(prefs),
            engine,
            active: None,
        }
    }

    /// The engine's name.
    pub fn engine_name(&self) -> &str {
        self.engine.name()
    }

    /// Makes the engine match the machine: stops a chunk that is no longer current (then
    /// silences the engine), pauses or resumes in place where the engine can (else stops the
    /// chunk, to say it again on resume), and starts the current chunk when none is being said.
    pub async fn sync(&mut self) {
        let want = self.machine.current().map(|(k, _)| k);
        let paused = self.machine.paused();
        if let Some(a) = &mut self.active {
            let keep = if Some(a.key) != want {
                false
            } else if paused && !a.native_paused {
                a.native_paused = self.engine.pause();
                a.native_paused
            } else if !paused && a.native_paused {
                a.native_paused = !self.engine.resume();
                !a.native_paused
            } else {
                true
            };
            if !keep {
                self.active = None;
                self.engine.after_stop().await;
            }
        }
        if self.active.is_none() && !paused {
            if let Some((key, text)) = self.machine.current() {
                let future = self.engine.speak(text, self.machine.prefs());
                self.active = Some(Active {
                    key,
                    future,
                    native_paused: false,
                });
            }
        }
    }

    /// Waits for the chunk being said to end, and tells the machine (then [`Player::sync`]
    /// starts the next). Pending while nothing is said. Cancel-safe: the chunk goes on.
    pub async fn next_done(&mut self) {
        let Some(a) = &mut self.active else {
            return futures::future::pending().await;
        };
        let result = (&mut a.future).await;
        let key = a.key;
        self.active = None;
        self.machine
            .chunk_done(key, result.map_err(|e| format!("{e:#}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{FakeEngine, FakeEvent};
    use std::time::Duration;

    fn msg(ws: &str, seq: u64, chunks: &[&str], origin: Origin) -> Msg {
        Msg {
            ws: ws.into(),
            thread: "T".into(),
            seq,
            chunks: chunks.iter().map(|c| c.to_string()).collect(),
            origin,
            batch: None,
            truncated: false,
        }
    }

    fn live(ws: &str, seq: u64, chunks: &[&str]) -> Msg {
        msg(ws, seq, chunks, Origin::Live)
    }

    fn req(ws: &str, seq: u64, batch: u64, chunks: &[&str]) -> Msg {
        Msg {
            batch: Some(batch),
            ..msg(ws, seq, chunks, Origin::Request)
        }
    }

    /// The machine's effects, a start's and a drop's alike (where it matters, tests check
    /// [`Effect::Started`] themselves).
    fn marks(m: &mut Machine) -> Vec<Effect> {
        m.take_effects()
            .into_iter()
            .map(|e| match e {
                Effect::Started { thread, seq } => Effect::Watermark { thread, seq },
                e => e,
            })
            .collect()
    }

    fn wm(seq: u64) -> Effect {
        Effect::Watermark {
            thread: "T".into(),
            seq,
        }
    }

    /// Says everything the machine has, chunk by chunk; the utterances in order.
    fn drain(m: &mut Machine) -> Vec<String> {
        let mut out = Vec::new();
        while let Some((k, t)) = m.current() {
            out.push(t.to_string());
            m.chunk_done(k, Ok(()));
        }
        out
    }

    #[test]
    fn messages_are_said_in_order_and_move_the_watermark_as_they_start() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.push(live("w", 3, &["a1", "a2"]));
        assert_eq!(marks(&mut m), vec![wm(3)]);
        m.push(live("w", 5, &["b"]));
        assert_eq!(marks(&mut m), vec![], "queued, not started");
        assert_eq!(m.queued(), 1);
        assert_eq!(drain(&mut m), vec!["a1", "a2", "b"]);
        assert_eq!(marks(&mut m), vec![wm(5)]);
        assert!(!m.busy());
    }

    /// A place handled that the watermark could not reach yet (a message waited before it) is
    /// reached once that message starts: a restart then doesn't say it again.
    #[test]
    fn a_place_held_back_is_reached_later() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.pause();
        m.replace("w", 1, 1);
        let mut req = live("w", 7, &["seven"]);
        req.origin = Origin::Request;
        req.batch = Some(1);
        m.push(req);
        m.push(live("w", 5, &["five"]));
        marks(&mut m);
        // The live copy of 7 comes after 5, which waits: held back at 4.
        m.push(live("w", 7, &["seven"]));
        assert_eq!(marks(&mut m), vec![wm(4)]);
        m.resume();
        assert_eq!(drain(&mut m), vec!["seven", "five"]);
        // 5 started: the watermark reaches 7.
        assert!(marks(&mut m).contains(&wm(7)));
    }

    /// A hub gone lets go of its waiting messages and of the places held back behind them:
    /// when they come again, the watermark doesn't pass one not yet read.
    #[test]
    fn a_hub_gone_drops_the_places_held_back() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.push(live("w", 3, &["three"]));
        m.push(live("w", 5, &["five"]));
        m.push(live("w", 6, &["six"]));
        m.push(live("w", 7, &[]));
        marks(&mut m);
        m.detached("w");
        m.set_live("w", true);
        assert_eq!(drain(&mut m), vec!["three"]);
        // 5 comes again (6 not yet): the watermark goes no further than 5.
        m.push(live("w", 5, &["five"]));
        let marks = marks(&mut m);
        assert!(!marks.contains(&wm(7)), "{marks:?}");
        assert!(marks.contains(&wm(5)), "{marks:?}");
    }

    /// A message asked for, then coming live (the feed behind the request): read once.
    #[test]
    fn a_message_asked_for_is_not_read_again_live() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.replace("w", 1, 1);
        let mut req = live("w", 7, &["it"]);
        req.origin = Origin::Request;
        req.batch = Some(1);
        m.push(req);
        marks(&mut m);
        m.push(live("w", 7, &["it"]));
        assert_eq!(drain(&mut m), vec!["it"]);
        // The live copy, dropped, still moves the watermark (a restart doesn't say it).
        assert_eq!(marks(&mut m), vec![wm(7)]);
        // Asked for again: read again.
        m.replace("w", 2, 1);
        let mut req = live("w", 7, &["it"]);
        req.origin = Origin::Request;
        req.batch = Some(2);
        m.push(req);
        assert_eq!(drain(&mut m), vec!["it"]);
    }

    /// Stopping, or asking for messages, ends a pause: what comes next plays.
    #[test]
    fn stop_and_a_request_end_a_pause() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.pause();
        m.stop();
        m.push(live("w", 3, &["a"]));
        assert_eq!(drain(&mut m), vec!["a"]);
        m.pause();
        m.replace("w", 1, 1);
        let mut req = live("w", 2, &["asked"]);
        req.origin = Origin::Request;
        req.batch = Some(1);
        m.push(req);
        assert_eq!(drain(&mut m), vec!["asked"]);
    }

    /// A hub gone: its messages not yet started go, their watermark unmoved, and are taken
    /// again when it sends them again; the one being said goes on.
    #[test]
    fn a_hub_gone_lets_its_waiting_messages_go() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.push(live("w", 3, &["a"]));
        m.push(live("w", 4, &["b"]));
        m.push(live("w", 5, &["c"]));
        assert_eq!(marks(&mut m), vec![wm(3)]);
        m.detached("w");
        assert_eq!(marks(&mut m), vec![], "no watermark for what was let go");
        assert_eq!(m.queued(), 0);
        // Attached again: its live switch comes with the attach.
        m.set_live("w", true);
        m.push(live("w", 4, &["b"]));
        assert_eq!(drain(&mut m), vec!["a", "b"]);
    }

    /// More than the de-duplication memory holds can wait (paused); a message sent again then
    /// is still known while it waits, and an announcement being said counts as waiting.
    #[test]
    fn what_waits_is_never_queued_twice() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.pause();
        for seq in 1..=(SEEN_MAX as u64 + 10) {
            m.push(live("w", seq, &["x"]));
        }
        let queued = m.queued();
        m.push(live("w", 1, &["x"]));
        assert_eq!(
            m.queued(),
            queued,
            "the first one, forgotten by the memory, still waits"
        );
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.missed("w", "T", 3, 9);
        assert_eq!(m.state_for("w").queued, 1, "the announcement is being said");
    }

    /// While a batch is still coming, a live message (of another workspace, or after the
    /// batch's first message was read) waits behind it; the batch's hub going away lets it go.
    #[test]
    fn a_batch_still_coming_goes_before_live_messages() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("b", true);
        m.replace("a", 1, 2);
        m.push(live("b", 9, &["live"]));
        assert_eq!(m.current(), None, "nothing starts before the batch");
        let mut req = live("a", 3, &["first"]);
        req.origin = Origin::Request;
        req.batch = Some(1);
        m.push(req);
        let step = |m: &mut Machine| {
            let (k, t) = m.current()?;
            let t = t.to_string();
            m.chunk_done(k, Ok(()));
            Some(t)
        };
        assert_eq!(step(&mut m).as_deref(), Some("Reading 2 messages."));
        assert_eq!(step(&mut m).as_deref(), Some("first"));
        // The batch's second message hasn't come: the live one still waits.
        assert_eq!(m.current(), None);
        m.detached("a");
        assert_eq!(drain(&mut m), vec!["live"]);

        // The batch's last message has nothing to say: what waits goes on at once.
        let mut m = Machine::new(Prefs::default());
        m.set_live("b", true);
        m.replace("a", 2, 1);
        m.push(live("b", 9, &["live"]));
        assert_eq!(m.current(), None);
        let mut empty = live("a", 3, &[]);
        empty.origin = Origin::Request;
        empty.batch = Some(2);
        m.push(empty);
        assert_eq!(drain(&mut m), vec!["live"]);
    }

    /// A message with nothing to say, behind one still waiting, moves the watermark only to
    /// just before the waiting one: a restart then reads the waiting one, never skips it.
    #[test]
    fn the_watermark_never_passes_a_waiting_message() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.push(live("w", 3, &["a"]));
        assert_eq!(marks(&mut m), vec![wm(3)]);
        m.push(live("w", 5, &["b"]));
        m.push(live("w", 7, &[]));
        assert_eq!(marks(&mut m), vec![wm(4)], "capped below the waiting 5");
        assert_eq!(drain(&mut m), vec!["a", "b"]);
        // 5 started: the watermark reaches the 7 held back (handled: nothing to say).
        assert_eq!(marks(&mut m), vec![wm(7)]);
    }

    #[test]
    fn live_off_drops_without_speaking_and_moves_the_watermark() {
        let mut m = Machine::new(Prefs::default());
        m.push(live("w", 3, &["a"]));
        assert!(m.current().is_none());
        assert_eq!(marks(&mut m), vec![wm(3)]);
        m.set_live("w", true);
        m.push(live("w", 4, &["b"]));
        m.push(live("w", 5, &["c"]));
        marks(&mut m);
        m.set_live("w", false);
        assert!(!m.busy());
        assert_eq!(marks(&mut m), vec![wm(5)]);
    }

    #[test]
    fn a_message_seen_before_is_dropped_but_a_request_is_not() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.push(live("w", 3, &["a"]));
        m.push(live("w", 3, &["a"]));
        assert_eq!(m.queued(), 0);
        assert_eq!(drain(&mut m), vec!["a"]);
        m.push(Msg {
            origin: Origin::Catchup,
            ..live("w", 3, &["a"])
        });
        assert!(!m.busy());
        m.replace("w", 1, 1);
        m.push(req("w", 3, 1, &["a"]));
        assert_eq!(drain(&mut m), vec!["a"]);
    }

    #[test]
    fn replace_stops_empties_and_reads_the_batch_before_later_live_messages() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.push(live("w", 1, &["old1", "old1b"]));
        m.push(live("w", 2, &["old2"]));
        let (k, _) = m.current().unwrap();
        m.chunk_done(k, Ok(()));
        marks(&mut m);
        m.replace("w", 7, 2);
        assert!(m.current().is_none());
        // The dropped queued message counts as handled.
        assert_eq!(marks(&mut m), vec![wm(2)]);
        m.push(req("w", 10, 7, &["r1"]));
        m.push(live("w", 12, &["new"]));
        m.push(req("w", 11, 7, &["r2"]));
        assert_eq!(m.state_for("w").batch, Some((1, 2)));
        assert_eq!(
            drain(&mut m),
            vec!["Reading 2 messages.", "r1", "r2", "new"]
        );
    }

    #[test]
    fn only_the_latest_batch_counts() {
        let mut m = Machine::new(Prefs::default());
        m.replace("w", 1, 2);
        m.push(req("w", 10, 1, &["one-a"]));
        m.replace("w", 2, 1);
        m.push(req("w", 11, 1, &["one-b"]));
        m.push(req("w", 20, 2, &["two"]));
        m.push(req("w", 21, 2, &["two-extra"]));
        m.push(req("x", 30, 2, &["other ws"]));
        m.push(Msg {
            batch: None,
            ..req("w", 40, 0, &["no batch"])
        });
        assert_eq!(drain(&mut m), vec!["two"]);
    }

    #[test]
    fn stop_skip_and_an_engine_error() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.push(live("w", 1, &["a1", "a2"]));
        m.push(live("w", 2, &["b"]));
        m.push(live("w", 3, &["c"]));
        m.skip();
        assert_eq!(m.current().unwrap().1, "b");
        marks(&mut m);
        m.stop();
        assert!(!m.busy());
        assert_eq!(marks(&mut m), vec![wm(3)]);
        m.push(live("w", 4, &["d"]));
        m.push(live("w", 5, &["e"]));
        let (k, _) = m.current().unwrap();
        m.chunk_done(k, Err("spd-say not found".into()));
        assert!(!m.busy());
        assert_eq!(m.error(), Some("spd-say not found"));
        assert_eq!(m.state_for("w").error.as_deref(), Some("spd-say not found"));
        m.push(live("w", 6, &["f"]));
        drain(&mut m);
        assert_eq!(m.error(), None);
    }

    #[test]
    fn prefaces() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("a", true);
        let long: Vec<String> = (0..10).map(|_| "word ".repeat(40)).collect();
        let long: Vec<&str> = long.iter().map(String::as_str).collect();
        m.push(Msg {
            truncated: true,
            ..live("a", 1, &long)
        });
        let said = drain(&mut m);
        assert_eq!(said[0], "long message, about 3 minutes.");
        assert_eq!(
            said.last().unwrap(),
            "message cut short; the rest is on screen."
        );
        // Two workspaces live: each message names its own.
        m.set_live("b", true);
        m.push(live("b", 2, &["hi"]));
        assert_eq!(drain(&mut m), vec!["b:", "hi"]);
        // A batch of one: no count.
        m.set_live("b", false);
        m.replace("a", 1, 1);
        m.push(req("a", 1, 1, &["again"]));
        assert_eq!(drain(&mut m), vec!["again"]);
    }

    #[test]
    fn missed_is_said_only_with_live_on_and_moves_the_watermark() {
        let mut m = Machine::new(Prefs::default());
        m.missed("w", "T", 4, 40);
        assert!(!m.busy());
        assert_eq!(marks(&mut m), vec![wm(40)]);
        m.set_live("w", true);
        m.missed("w", "T", 4, 44);
        assert_eq!(
            drain(&mut m),
            vec!["4 earlier messages came while I was disconnected; they're on screen"]
        );
        assert_eq!(marks(&mut m), vec![wm(44)]);
    }

    #[test]
    fn state_names_only_the_workspace_it_plays_for() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("a", true);
        m.set_live("b", true);
        m.push(live("a", 1, &["x", "y"]));
        m.push(Msg {
            thread: "U".into(),
            ..live("b", 2, &["z"])
        });
        let a = m.state_for("a");
        assert_eq!(
            a.playing,
            Some(Playing {
                thread: "T".into(),
                seq: 1,
                chunk: 1,
                of: 3
            })
        );
        assert_eq!(a.queued, 0);
        let b = m.state_for("b");
        assert_eq!((b.playing, b.queued, b.live), (None, 1, true));
    }

    /// A batch some of whose messages never come (a newer hub's, which this speaker can't
    /// read) holds what waits behind it for [`BATCH_IDLE_TICKS`] ticks without a message of
    /// it, then ends with what came.
    #[test]
    fn a_batch_whose_messages_stop_coming_ends_after_idle_ticks() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("w", true);
        m.replace("w", 1, 3);
        m.push(live("w", 9, &["live"]));
        for _ in 0..BATCH_IDLE_TICKS - 1 {
            m.tick();
        }
        assert_eq!(m.current(), None, "still waiting for the batch");
        // A message of it starts the count again.
        m.push(req("w", 10, 1, &["first"]));
        assert_eq!(m.state_for("w").batch, Some((1, 3)));
        assert_eq!(drain(&mut m), vec!["Reading 3 messages.", "first"]);
        for _ in 0..BATCH_IDLE_TICKS - 1 {
            m.tick();
        }
        assert_eq!(
            m.current(),
            None,
            "the second hasn't come, nor a tick too many"
        );
        m.tick();
        assert_eq!(drain(&mut m), vec!["live"]);
        // Ended: a late message of it is dropped, and ticks do nothing more.
        m.push(req("w", 11, 1, &["late"]));
        m.tick();
        assert!(!m.busy());
    }

    /// Two requests reaching the speaker the other way round: the newer batch (the higher id)
    /// is read, the older one's replace and messages are dropped; a hub attached again (a
    /// restarted one numbers from 1) is heard again.
    #[test]
    fn an_older_batch_coming_last_is_ignored() {
        let mut m = Machine::new(Prefs::default());
        m.replace("w", 2, 1);
        m.replace("w", 1, 1);
        m.push(req("w", 10, 1, &["older"]));
        m.push(req("w", 20, 2, &["newer"]));
        assert_eq!(drain(&mut m), vec!["newer"]);
        // Another workspace's hub numbers its own.
        m.replace("x", 1, 1);
        m.push(req("x", 30, 1, &["other hub"]));
        assert_eq!(drain(&mut m), vec!["other hub"]);
        m.detached("w");
        m.replace("w", 1, 1);
        m.push(req("w", 40, 1, &["after a restart"]));
        assert_eq!(drain(&mut m), vec!["after a restart"]);
    }

    /// A hub gone no longer counts among the workspaces reading live: messages of the one left
    /// aren't prefaced with its name.
    #[test]
    fn a_hub_gone_no_longer_counts_as_reading_live() {
        let mut m = Machine::new(Prefs::default());
        m.set_live("a", true);
        m.set_live("b", true);
        m.push(live("a", 1, &["hi"]));
        assert_eq!(drain(&mut m), vec!["a:", "hi"]);
        m.detached("b");
        assert!(!m.live("b"));
        m.push(live("a", 2, &["again"]));
        assert_eq!(drain(&mut m), vec!["again"]);
    }

    /// Runs the player until it has nothing more to say, or `limit` virtual time passed.
    async fn run_for(p: &mut Player, limit: Duration) {
        let end = tokio::time::Instant::now() + limit;
        loop {
            p.sync().await;
            tokio::select! {
                _ = p.next_done() => {}
                _ = tokio::time::sleep_until(end) => return,
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stop_silences_within_the_chunk() {
        let (e, h) = FakeEngine::timed(Duration::from_secs(5));
        let mut p = Player::new(Box::new(e), Prefs::default());
        p.machine.set_live("w", true);
        p.machine.push(live("w", 1, &["a", "b"]));
        run_for(&mut p, Duration::from_secs(1)).await;
        assert!(h.speaking());
        p.machine.stop();
        p.sync().await;
        // At once: no time passed.
        assert_eq!(h.events().last(), Some(&FakeEvent::Stopped("a".into())));
        assert!(!h.speaking());
        run_for(&mut p, Duration::from_secs(20)).await;
        assert_eq!(h.spoken(), vec!["a"]);
    }

    #[tokio::test(start_paused = true)]
    async fn pause_then_resume_says_the_chunk_again_or_goes_on_in_place() {
        // An engine that can't pause: the chunk is stopped, then said again.
        let (e, h) = FakeEngine::timed(Duration::from_secs(2));
        let mut p = Player::new(Box::new(e), Prefs::default());
        p.machine.set_live("w", true);
        p.machine.push(live("w", 1, &["a", "b"]));
        run_for(&mut p, Duration::from_secs(3)).await;
        assert_eq!(h.spoken(), vec!["a", "b"]);
        p.machine.pause();
        p.sync().await;
        assert_eq!(h.events().last(), Some(&FakeEvent::Stopped("b".into())));
        run_for(&mut p, Duration::from_secs(60)).await;
        assert_eq!(h.spoken(), vec!["a", "b"], "nothing while paused");
        p.machine.resume();
        run_for(&mut p, Duration::from_secs(60)).await;
        assert_eq!(h.spoken(), vec!["a", "b", "b"]);
        assert_eq!(h.finished(), vec!["a", "b"]);

        // One that can: paused in place, no restart.
        let (e, h) = FakeEngine::timed(Duration::from_secs(2));
        let mut p = Player::new(Box::new(e.with_native_pause()), Prefs::default());
        p.machine.set_live("w", true);
        p.machine.push(live("w", 1, &["a"]));
        run_for(&mut p, Duration::from_secs(1)).await;
        p.machine.pause();
        run_for(&mut p, Duration::from_secs(60)).await;
        assert!(h.finished().is_empty());
        p.machine.resume();
        run_for(&mut p, Duration::from_secs(60)).await;
        assert_eq!(h.spoken(), vec!["a"]);
        assert_eq!(h.finished(), vec!["a"]);
        assert!(h.events().contains(&FakeEvent::Paused));
        assert!(h.events().contains(&FakeEvent::Resumed));
    }

    #[tokio::test(start_paused = true)]
    async fn skip_ends_the_message_and_goes_on() {
        let (e, h) = FakeEngine::timed(Duration::from_secs(2));
        let mut p = Player::new(Box::new(e), Prefs::default());
        p.machine.set_live("w", true);
        p.machine.push(live("w", 1, &["a1", "a2"]));
        p.machine.push(live("w", 2, &["b"]));
        run_for(&mut p, Duration::from_secs(1)).await;
        p.machine.skip();
        run_for(&mut p, Duration::from_secs(60)).await;
        assert_eq!(h.spoken(), vec!["a1", "b"]);
        assert_eq!(h.finished(), vec!["b"]);
    }

    #[tokio::test(start_paused = true)]
    async fn voice_and_rate_apply_from_the_next_chunk() {
        let (e, h) = FakeEngine::timed(Duration::from_secs(2));
        let mut p = Player::new(Box::new(e), Prefs::default());
        p.machine.set_live("w", true);
        p.machine.push(live("w", 1, &["a", "b"]));
        run_for(&mut p, Duration::from_secs(1)).await;
        p.machine.set_voice(Some("Ava".into()));
        p.machine.set_rate(220);
        run_for(&mut p, Duration::from_secs(60)).await;
        let prefs: Vec<Prefs> = h
            .events()
            .into_iter()
            .filter_map(|e| match e {
                FakeEvent::Speak { prefs, .. } => Some(prefs),
                _ => None,
            })
            .collect();
        assert_eq!(prefs[0], Prefs::default());
        assert_eq!(prefs[1].voice.as_deref(), Some("Ava"));
        assert_eq!(prefs[1].rate, 220);
    }

    #[tokio::test(start_paused = true)]
    async fn an_engine_failure_drops_the_queue() {
        let (e, h) = FakeEngine::timed(Duration::from_secs(2));
        let mut p = Player::new(Box::new(e), Prefs::default());
        p.machine.set_live("w", true);
        h.fail_next("say: not found");
        p.machine.push(live("w", 1, &["a"]));
        p.machine.push(live("w", 2, &["b"]));
        run_for(&mut p, Duration::from_secs(60)).await;
        assert_eq!(h.spoken(), vec!["a"]);
        assert_eq!(p.machine.error(), Some("say: not found"));
        assert!(!p.machine.busy());
    }
}
