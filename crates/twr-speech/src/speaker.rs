//! The speaker: a [`Link`] to hubs, a [`Player`] and the device's [`SpeechState`], tied
//! together by [`run`].
//!
//! - **Attach.** Each workspace's attach carries every watermark the state knows (a hub ignores
//!   threads it doesn't have), the device's live switch for that workspace, `read`,
//!   `turn_from` and `catchup_secs`. With no watermark yet for the orchestrator the ack names,
//!   its head is saved as one at once, and an `orchestrator` item saves the new thread's start
//!   (0): a disconnection before the first message loses nothing.
//! - **Items.** A message is read as utterances (`twr_core::speech::utterances`) and goes to the
//!   player; controls act on it (`live`, `voice` and `rate` are saved too); `missed` is said.
//! - **State.** After every change each connected workspace's hub is told what this speaker
//!   does for it (`speech.state`): what it plays only to the hub it came from.
//! - **Keys** ([`tty_keys`], `twr speak` in a terminal): `s` stop, `p` pause or resume, `n`
//!   skip, `q` (or Ctrl-C) quit.

use std::collections::BTreeSet;
use std::future::Future;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::bail;
use serde_json::Value;
use tokio::sync::mpsc;
use twr_core::config::SpeechConfig;
use twr_core::speech::{
    name_error, utterances, CtlOp, SpeakOpts, SpeechAttachParams, SpeechItem, TEXT_MAX,
};

use crate::engine::{Engine, Prefs};
use crate::link::{Link, LinkEvent, LinkSpec, ParamsFn};
use crate::player::{Effect, Msg, Player};
use crate::state::{live_key, SpeechState, StateStore};

/// Where the speaker's notes go (attached, standing by, an engine error, ssh's own words).
pub type Reporter = Arc<dyn Fn(&str) + Send + Sync>;

/// How a speaker runs.
#[derive(Clone)]
pub struct SpeakerOpts {
    /// Its name (`/read status`, `--on`): 1-64 printable characters, the host's name by
    /// default.
    pub name: String,
    /// `[speech]`.
    pub config: SpeechConfig,
    /// A voice that beats the saved one and `[speech] voice` (`--voice`).
    pub voice: Option<String>,
    /// A rate that beats the saved one and `[speech] rate` (`--rate`).
    pub rate: Option<u32>,
    /// The device's state file.
    pub state: StateStore,
    /// Where notes go; `None`: the log (`tracing`).
    pub report: Option<Reporter>,
    /// Read this terminal's keys ([`tty_keys`]) once a hub is attached, not before: until then
    /// ssh may ask for a passphrase or about the host key on the same terminal, and a key
    /// reader in raw mode would take what you type. With `keys` given, ignored.
    pub tty_keys_once_attached: bool,
}

impl std::fmt::Debug for SpeakerOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeakerOpts")
            .field("name", &self.name)
            .field("voice", &self.voice)
            .field("rate", &self.rate)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// A key of the speaker's own terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// `s`: stop and empty the queue.
    Stop,
    /// `p`: pause, or resume.
    PauseResume,
    /// `n`: the next message.
    Skip,
    /// `q`, Ctrl-C: quit.
    Quit,
}

/// Keys for [`run`].
pub type KeyInput = mpsc::Receiver<Key>;

/// A change to save in the state file.
type Change = Box<dyn Fn(&mut SpeechState) + Send>;

/// How often a speaker standing by for another one of this machine tries to take over.
pub const OWNER_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// The rates a speaker takes, in words per minute (`--rate`, `/read rate`).
pub const RATES: std::ops::RangeInclusive<u32> = 80..=500;

/// The longest voice name a speaker takes, in characters (as a hub does).
pub const VOICE_MAX: usize = 128;

/// What [`run`]'s parts share: the state as last saved, and the voice it gives (for attach
/// params and each attach).
struct Shared {
    state: SpeechState,
    voice: Option<String>,
    /// `--voice`, which beats the saved one.
    voice_flag: Option<String>,
    /// `[speech] voice`, if set: the voice without a saved one.
    cfg_voice: Option<String>,
    /// Changes not saved yet (the last write failed: a full disk): applied to the state read
    /// from the file wherever it is read, and saved again with the next save.
    unsaved: Vec<Change>,
}

impl Shared {
    /// `state` is the state file as it is now: it, and the voice it gives (`--voice`, else
    /// the saved one, else `[speech] voice`), become the shared ones. Called after every save
    /// and every read of the file, so the voice never lags behind the file (a `/read voice`
    /// saved while an attach read the file before it).
    fn adopt(&mut self, mut state: SpeechState) {
        for c in &self.unsaved {
            c(&mut state);
        }
        self.voice = self
            .voice_flag
            .clone()
            .or_else(|| state.voice.clone())
            .or_else(|| self.cfg_voice.clone());
        self.state = state;
    }
}

/// Runs a speaker on `link` with `engine` until `shutdown` resolves or `q` is pressed
/// (`keys`; `None`: no keys, as in a TUI). Fails at start on a bad name or an unusable state
/// file; afterwards errors are reported and the speaker goes on.
pub async fn run(
    opts: SpeakerOpts,
    link: LinkSpec,
    engine: Box<dyn Engine>,
    mut keys: Option<KeyInput>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    if let Some(e) = name_error("name", &opts.name, 64) {
        bail!("speaker {e}");
    }
    // Every hub would refuse the attach of a voice it can't take, again and again.
    if let Some(e) = opts
        .voice
        .as_deref()
        .and_then(|v| name_error("voice", v, VOICE_MAX))
    {
        bail!("speaker {e}");
    }
    if let Some(r) = opts.rate.filter(|r| !RATES.contains(r)) {
        bail!(
            "speaker rate {r}: {} to {} words per minute",
            RATES.start(),
            RATES.end()
        );
    }
    let report: Reporter = opts
        .report
        .clone()
        .unwrap_or_else(|| Arc::new(|note: &str| tracing::info!("{note}")));
    let store = opts.state.clone();
    let state = store.load()?;
    let cfg = &opts.config;
    let prefs = Prefs {
        voice: opts
            .voice
            .clone()
            .or_else(|| state.voice.clone())
            .or_else(|| (!cfg.voice.is_empty()).then(|| cfg.voice.clone())),
        rate: opts.rate.or(state.rate).unwrap_or(cfg.rate),
        volume: cfg.volume,
    };
    let speak = SpeakOpts {
        code: cfg.code,
        paths: cfg.paths,
    };
    let host = link.host();
    let shared = Arc::new(Mutex::new(Shared {
        state,
        voice: prefs.voice.clone(),
        voice_flag: opts.voice.clone(),
        cfg_voice: (!cfg.voice.is_empty()).then(|| cfg.voice.clone()),
        unsaved: Vec::new(),
    }));
    let params: ParamsFn = {
        let (shared, host) = (shared.clone(), host.clone());
        let (name, engine_name) = (opts.name.clone(), engine.name().to_string());
        let (read, turn_from, catchup) = (cfg.read.clone(), cfg.turn_from, cfg.catchup_secs);
        let store = store.clone();
        Arc::new(move |ws: &str| {
            let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
            // The file as it is now: another speaker of this device (the one this one stood
            // by for) may have read on and switched live reading since. Every change here is
            // saved at once (before the engine is waited on), so the file holds this one's
            // too; one made since is saved next, and the shared voice follows that save.
            match store.load() {
                Ok(fresh) => s.adopt(fresh),
                Err(e) => tracing::debug!("speaker: reading its state again: {e:#}"),
            }
            SpeechAttachParams {
                device: s.state.device.clone(),
                name: name.clone(),
                engine: engine_name.clone(),
                voice: s.voice.clone(),
                live: s.state.live(&host, ws),
                read: read.clone(),
                turn_from,
                watermarks: s.state.watermarks.clone(),
                catchup_secs: catchup,
            }
        })
    };
    // One speaker of this device speaks for a machine's hubs: another one for the same hubs
    // (the TUI's own, `twr speak`), whichever of them it reaches, makes this one stand by
    // until it lets go.
    tokio::pin!(shutdown);
    let mut said_standby = false;
    let _owner = loop {
        match store.try_own(&host) {
            Ok(Some(owner)) => break Some(owner),
            Ok(None) => {
                if !said_standby {
                    report("standing by: another speaker of this machine speaks for these hubs; this one takes over when it stops");
                    said_standby = true;
                }
                tokio::select! {
                    _ = &mut shutdown => return Ok(()),
                    () = tokio::time::sleep(OWNER_RETRY) => {}
                }
            }
            Err(e) => {
                report(&format!(
                    "speaker: {e:#}; speaking without telling other speakers of this machine"
                ));
                break None;
            }
        }
    };
    if said_standby {
        report("this speaker speaks now");
    }
    // A place to start from (an attach's head, a new orchestrator's start) is saved by the
    // link before it passes it on, so an attach made meanwhile (a quick reconnect) has it.
    let persist: crate::link::PersistFn = {
        let (store, shared) = (store.clone(), shared.clone());
        Arc::new(move |_ws: &str, p: crate::link::Persist<'_>| {
            let (thread, seq) = match p {
                crate::link::Persist::Attached(ack) => match &ack.orchestrator {
                    Some(o) => (o.clone(), ack.head),
                    None => return,
                },
                crate::link::Persist::Orchestrator(t) => (t.to_string(), 0),
            };
            // The shared state held through the write and its adoption, as the speaker's own
            // saves hold it: states are adopted in the order they were written.
            let mut sh = shared.lock().unwrap_or_else(|p| p.into_inner());
            match store.update(|s| s.init_watermark(&thread, seq)) {
                Ok(s) => sh.adopt(s),
                Err(e) => {
                    tracing::warn!("speaker: saving where {thread} starts: {e:#}");
                    // Kept, and saved with the next save.
                    sh.unsaved
                        .push(Box::new(move |s| s.init_watermark(&thread, seq)));
                    let st = sh.state.clone();
                    sh.adopt(st);
                }
            }
        })
    };
    let mut link = Link::start_with(link, params, persist);
    // The keys read once a hub is attached ([`SpeakerOpts::tty_keys_once_attached`]), and
    // the terminal's raw mode while they are.
    let mut want_keys = opts.tty_keys_once_attached && keys.is_none();
    let mut key_guard: Option<KeyGuard> = None;
    let mut player = Player::new(engine, prefs);
    let mut connected: BTreeSet<String> = BTreeSet::new();
    let mut said_error: Option<String> = None;
    let mut saver = Saver {
        store: store.clone(),
        shared: shared.clone(),
        report: report.clone(),
        failed: false,
        starts: Vec::new(),
    };
    // The machine keeps no clock: it is told each second ([`Machine::tick`]: a batch whose
    // messages stopped coming ends).
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // Changes to save, gathered from this round.
        let mut changes: Vec<Change> = Vec::new();
        tokio::select! {
            _ = &mut shutdown => break,
            key = next_key(&mut keys) => match key {
                Some(Key::Quit) => break,
                Some(Key::Stop) => player.machine.stop(),
                Some(Key::PauseResume) => player.machine.toggle_pause(),
                Some(Key::Skip) => player.machine.skip(),
                // The key reader ended (the terminal went away): the terminal goes back out
                // of raw mode with it.
                None => {
                    keys = None;
                    key_guard = None;
                }
            },
            ev = link.next() => {
                let Some(ev) = ev else { break };
                match ev {
                    LinkEvent::Attached { ws, ack } => {
                        if want_keys {
                            want_keys = false;
                            match tty_keys() {
                                Ok(Some((k, g))) => {
                                    keys = Some(k);
                                    key_guard = Some(g);
                                    report("keys: s stop · p pause · n skip · q quit");
                                }
                                Ok(None) => {}
                                Err(e) => report(&format!("no keys: {e:#}")),
                            }
                        }
                        // As last saved or read: the voice follows every save ([`Shared::adopt`]).
                        let (live, voice, rate) = {
                            let s = shared.lock().unwrap_or_else(|p| p.into_inner());
                            (s.state.live(&host, &ws), s.voice.clone(), s.state.rate)
                        };
                        player.machine.set_live(&ws, live);
                        // The voice and rate as the state file has them now (another speaker
                        // of this device may have changed them), unless given on the command
                        // line.
                        if player.machine.prefs().voice != voice {
                            player.machine.set_voice(voice);
                        }
                        let rate = opts.rate.or(rate).unwrap_or(opts.config.rate);
                        if player.machine.prefs().rate != rate {
                            player.machine.set_rate(rate);
                        }
                        if let Some(orch) = ack.orchestrator {
                            let head = ack.head;
                            changes.push(Box::new(move |s| s.init_watermark(&orch, head)));
                        }
                        report(&format!(
                            "{}: connected; live reading {}",
                            place(&host, &ws),
                            if live { "on" } else { "off" }
                        ));
                        connected.insert(ws);
                    }
                    LinkEvent::Detached { ws, why } => {
                        report(&format!("{}: disconnected ({why})", place(&host, &ws)));
                        connected.remove(&ws);
                        player.machine.detached(&ws);
                    }
                    LinkEvent::Note(n) => report(&n),
                    LinkEvent::Item { ws, item } => {
                        item_to_player(&mut player, &ws, &host, item, &speak, &mut changes);
                    }
                }
            }
            _ = player.next_done() => {}
            _ = tick.tick() => player.machine.tick(),
        }
        // Saved before the engine is waited on: `sync` may wait for `after_stop` (`spd-say
        // -C`, up to 2 s), and a speaker aborted meanwhile (the TUI's, on quit) would lose
        // watermarks and switches. A message that starts is saved after it, once the engine
        // says it: aborted before, it isn't counted as read.
        saver.save(&mut player, changes, false);
        player.sync().await;
        saver.save(&mut player, Vec::new(), true);
        let error = player.machine.error().map(str::to_string);
        if error.is_some() && error != said_error {
            report(&format!(
                "speaker: {}",
                error.as_deref().unwrap_or_default()
            ));
        }
        said_error = error;
        for ws in &connected {
            link.report(ws, player.machine.state_for(ws));
        }
    }
    // Quitting stops the voice, also what speech-dispatcher still has of the chunk.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), player.silence()).await;
    drop(key_guard);
    Ok(())
}

/// Saves what a round of [`run`] changed.
struct Saver {
    store: StateStore,
    shared: Arc<Mutex<Shared>>,
    report: Reporter,
    /// The last save failed (said once until one works).
    failed: bool,
    /// Starts not saved yet: saved once the engine said the start ([`Effect::Started`]).
    starts: Vec<(String, u64)>,
}

impl Saver {
    /// Saves `changes` and the watermarks the machine asks for; the state saved becomes the
    /// shared one ([`Shared::adopt`]). Nothing to save, no write.
    fn save(&mut self, player: &mut Player, mut changes: Vec<Change>, started: bool) {
        for e in player.machine.take_effects() {
            match e {
                Effect::Watermark { thread, seq } => {
                    changes.push(Box::new(move |s| s.set_watermark(&thread, seq)))
                }
                Effect::Started { thread, seq } => self.starts.push((thread, seq)),
            }
        }
        // Once the engine is saying what starts (`started`): its watermark too.
        if started {
            for (thread, seq) in self.starts.drain(..) {
                changes.push(Box::new(move |s| s.set_watermark(&thread, seq)));
            }
        }
        // What a failed save left is saved first (each change applies again as it did). The
        // shared state is held through the write and its adoption, as every other save holds
        // it (the link's, [`Persist`]): states are adopted in the order they were written.
        let mut sh = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        let mut all = std::mem::take(&mut sh.unsaved);
        all.extend(changes);
        if all.is_empty() {
            return;
        }
        match self.store.update(|s| all.iter().for_each(|c| c(s))) {
            Ok(s) => {
                sh.adopt(s);
                if self.failed {
                    (self.report)("speaker: its state is saved again");
                }
                self.failed = false;
            }
            Err(e) => {
                if !self.failed {
                    (self.report)(&format!("speaker: saving its state: {e:#} (tried again)"));
                }
                self.failed = true;
                // Kept: in effect here (attaches and live reading follow them), saved with the
                // next save.
                sh.unsaved = all;
                let st = sh.state.clone();
                sh.adopt(st);
            }
        }
    }
}

/// Where a workspace is, for notes: `ws`, or `host:ws` over SSH.
fn place(host: &str, ws: &str) -> String {
    if host == crate::state::LOCAL_HOST {
        ws.to_string()
    } else {
        format!("{host}:{ws}")
    }
}

/// The next key; `None` once the keys are gone (pending without keys).
async fn next_key(keys: &mut Option<KeyInput>) -> Option<Key> {
    match keys {
        Some(k) => k.recv().await,
        None => futures::future::pending().await,
    }
}

/// Hands an item of `ws`'s feed to the player; what to save goes to `changes`.
fn item_to_player(
    player: &mut Player,
    ws: &str,
    host: &str,
    item: SpeechItem,
    speak: &SpeakOpts,
    changes: &mut Vec<Change>,
) {
    let m = &mut player.machine;
    match item {
        SpeechItem::Msg {
            thread,
            seq,
            mut text,
            mut truncated,
            origin,
            batch,
            ..
        } => {
            // No more than a hub sends, whatever this one sent.
            if text.len() > TEXT_MAX {
                let mut end = TEXT_MAX;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                truncated = true;
            }
            m.push(Msg {
                ws: ws.to_string(),
                thread,
                seq,
                chunks: utterances(&text, speak),
                origin,
                batch,
                truncated,
            })
        }
        SpeechItem::Ctl { op, value } => match op {
            CtlOp::Stop => m.stop(),
            CtlOp::Pause => m.pause(),
            CtlOp::Resume => m.resume(),
            CtlOp::Skip => m.skip(),
            CtlOp::Replace => {
                let v = value.unwrap_or(Value::Null);
                match (v["batch"].as_u64(), v["count"].as_u64()) {
                    (Some(batch), Some(count)) => {
                        m.replace(ws, batch, u32::try_from(count).unwrap_or(u32::MAX))
                    }
                    _ => tracing::debug!("a replace without batch and count: {v}"),
                }
            }
            CtlOp::Live => {
                if let Some(on) = value.as_ref().and_then(Value::as_bool) {
                    m.set_live(ws, on);
                    let key = live_key(host, ws);
                    changes.push(Box::new(move |s| {
                        s.live.insert(key.clone(), on);
                    }));
                }
            }
            // A voice or rate a hub checked, checked again: saved, a bad one would be sent to
            // every hub at each attach (and refused there).
            CtlOp::Voice => match value.as_ref().and_then(Value::as_str) {
                Some(v) if v.is_empty() || name_error("voice", v, VOICE_MAX).is_none() => {
                    let v = (!v.is_empty()).then(|| v.to_string());
                    m.set_voice(v.clone());
                    // The shared voice follows once this is saved ([`Shared::adopt`]).
                    changes.push(Box::new(move |s| s.voice = v.clone()));
                }
                _ => tracing::debug!("ignoring a voice control: {value:?}"),
            },
            CtlOp::Rate => match value
                .as_ref()
                .and_then(Value::as_u64)
                .and_then(|r| u32::try_from(r).ok())
                .filter(|r| RATES.contains(r))
            {
                Some(r) => {
                    m.set_rate(r);
                    changes.push(Box::new(move |s| s.rate = Some(r)));
                }
                None => tracing::debug!("ignoring a rate control: {value:?}"),
            },
        },
        SpeechItem::Missed {
            thread,
            count,
            upto,
        } => m.missed(ws, &thread, count, upto),
        SpeechItem::Orchestrator { thread } => {
            // The feed reads a new orchestrator from its start.
            changes.push(Box::new(move |s| s.init_watermark(&thread, 0)));
        }
    }
}

/// Whether the terminal is in raw mode for [`tty_keys`] (the panic hook restores it).
static RAW: AtomicBool = AtomicBool::new(false);

/// Whether [`tty_keys`] has the terminal in raw mode now: a line printed then ends with
/// `\r\n`.
pub fn raw_mode() -> bool {
    RAW.load(Ordering::SeqCst)
}

/// Installs, once, a panic hook that leaves raw mode before the panic is printed.
fn restore_on_panic() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if RAW.swap(false, Ordering::SeqCst) {
                let _ = crossterm::terminal::disable_raw_mode();
            }
            prev(info);
        }));
    });
}

/// The speaker's keys while it lives: raw mode, and the thread reading them. Dropping it ends
/// both.
pub struct KeyGuard {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for KeyGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyGuard").finish_non_exhaustive()
    }
}

impl Drop for KeyGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if RAW.swap(false, Ordering::SeqCst) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// The terminal's keys ([`Key`]), when stdin is a terminal: it goes into raw mode (left on
/// drop of the guard, and on a panic), so lines printed meanwhile need `\r\n`. `None` without a
/// terminal.
pub fn tty_keys() -> anyhow::Result<Option<(KeyInput, KeyGuard)>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    restore_on_panic();
    crossterm::terminal::enable_raw_mode()?;
    RAW.store(true, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel(16);
    let stop = Arc::new(AtomicBool::new(false));
    let st = stop.clone();
    let thread = std::thread::Builder::new()
        .name("twr-speak-keys".into())
        .spawn(move || {
            while !st.load(Ordering::SeqCst) {
                match event::poll(std::time::Duration::from_millis(100)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => return,
                }
                let Ok(Event::Key(k)) = event::read() else {
                    continue;
                };
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                let key = match k.code {
                    KeyCode::Char('c' | 'd') if ctrl => Key::Quit,
                    KeyCode::Char('q') => Key::Quit,
                    KeyCode::Char('s') => Key::Stop,
                    KeyCode::Char('p') | KeyCode::Char(' ') => Key::PauseResume,
                    KeyCode::Char('n') => Key::Skip,
                    _ => continue,
                };
                // Never waits: a key pressed while 16 wait is dropped (the speaker is busy;
                // press it again), so the reader always sees `stop` and quitting never hangs.
                match tx.try_send(key) {
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                    Err(mpsc::error::TrySendError::Full(_)) => continue,
                    Ok(()) if key == Key::Quit => return,
                    Ok(()) => {}
                }
            }
        });
    // No reader, no raw mode: the terminal goes back as it was.
    let thread = match thread {
        Ok(t) => t,
        Err(e) => {
            let _ = crossterm::terminal::disable_raw_mode();
            RAW.store(false, Ordering::SeqCst);
            return Err(e.into());
        }
    };
    Ok(Some((
        rx,
        KeyGuard {
            stop,
            thread: Some(thread),
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{FakeEngine, FakeEvent, FakeHandle};
    use crate::link::{manual, ManualEvent, ManualHub};
    use serde_json::json;
    use std::time::Duration;
    use twr_core::speech::{MsgKind, Origin, SpeechAttachAck};

    /// A speaker on a manual link with a fake engine, its state in a tempdir.
    struct Rig {
        hub: ManualHub,
        fake: FakeHandle,
        store: StateStore,
        keys: mpsc::Sender<Key>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
        notes: Arc<Mutex<Vec<String>>>,
    }

    fn opts(store: &StateStore, notes: &Arc<Mutex<Vec<String>>>) -> SpeakerOpts {
        let notes = notes.clone();
        SpeakerOpts {
            tty_keys_once_attached: false,
            name: "test speaker".into(),
            config: SpeechConfig::default(),
            voice: None,
            rate: None,
            state: store.clone(),
            report: Some(Arc::new(move |n: &str| {
                notes.lock().unwrap().push(n.to_string())
            })),
        }
    }

    fn start(store: &StateStore, (engine, fake): (FakeEngine, FakeHandle)) -> Rig {
        let (link, hub) = manual();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (keys, key_rx) = mpsc::channel(8);
        let notes = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(run(
            opts(store, &notes),
            LinkSpec::Manual(link),
            Box::new(engine),
            Some(key_rx),
            async move {
                let _ = stopped.await;
            },
        ));
        Rig {
            hub,
            fake,
            store: store.clone(),
            keys,
            stop: Some(stop),
            task,
            notes,
        }
    }

    impl Rig {
        fn send(&self, ev: ManualEvent) {
            self.hub.events.send(ev).unwrap();
        }

        /// `ws`'s hub attaches the speaker; the params it sent.
        async fn connect(
            &mut self,
            ws: &str,
            orchestrator: Option<&str>,
            head: u64,
        ) -> SpeechAttachParams {
            self.send(ManualEvent::Connect {
                ws: ws.into(),
                ack: SpeechAttachAck {
                    orchestrator: orchestrator.map(str::to_string),
                    head,
                },
            });
            let (w, p) = self.hub.attaches.recv().await.unwrap();
            assert_eq!(w, ws);
            p
        }

        fn item(&self, ws: &str, item: SpeechItem) {
            self.send(ManualEvent::Item {
                ws: ws.into(),
                item,
            });
        }

        fn state(&self) -> SpeechState {
            self.store.load().unwrap()
        }

        async fn shut(mut self) {
            let _ = self.stop.take().unwrap().send(());
            self.task.await.unwrap().unwrap();
        }
    }

    fn msg(seq: u64, text: &str, origin: Origin) -> SpeechItem {
        SpeechItem::Msg {
            thread: "T".into(),
            seq,
            turn: Some(1),
            ts: "2026-10-27T12:00:00Z".into(),
            kind: MsgKind::Reply,
            text: text.into(),
            truncated: false,
            origin,
            batch: None,
        }
    }

    fn req(seq: u64, text: &str, batch: u64) -> SpeechItem {
        match msg(seq, text, Origin::Request) {
            SpeechItem::Msg {
                thread,
                seq,
                turn,
                ts,
                kind,
                text,
                truncated,
                origin,
                ..
            } => SpeechItem::Msg {
                thread,
                seq,
                turn,
                ts,
                kind,
                text,
                truncated,
                origin,
                batch: Some(batch),
            },
            _ => unreachable!(),
        }
    }

    fn ctl(op: CtlOp, value: Option<Value>) -> SpeechItem {
        SpeechItem::Ctl { op, value }
    }

    /// Waits (virtual time) until `f` holds.
    async fn until(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..2000 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never: {what}");
    }

    fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::in_data_dir(dir.path());
        (dir, store)
    }

    fn live_on(store: &StateStore, ws: &str) {
        store
            .update(|s| {
                s.live.insert(live_key("local", ws), true);
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_message_is_read_and_its_watermark_saved_as_it_starts() {
        let (_d, store) = store();
        let mut r = start(&store, FakeEngine::manual());
        let p = r.connect("default", Some("T"), 3).await;
        assert!(p.watermarks.is_empty() && !p.live);
        assert_eq!(p.name, "test speaker");
        assert_eq!(p.engine, "fake");
        // The head is saved at once: nothing said meanwhile is lost.
        until("head saved", || r.state().watermark("T") == Some(3)).await;
        // Live off: a message counts as read, unsaid.
        r.item("default", msg(4, "Not said.", Origin::Live));
        until("4 handled", || r.state().watermark("T") == Some(4)).await;
        r.item("default", ctl(CtlOp::Live, Some(json!(true))));
        until("live saved", || r.state().live("local", "default")).await;
        r.item("default", msg(5, "Hello **there**.", Origin::Live));
        until("speaking", || r.fake.speaking()).await;
        assert_eq!(r.fake.spoken(), vec!["Hello there."]);
        assert_eq!(r.state().watermark("T"), Some(5), "saved as it started");
        // The hub hears what plays.
        let mut playing = None;
        while let Ok((ws, s)) = r.hub.states.try_recv() {
            assert_eq!(ws, "default");
            playing = s.playing.or(playing);
        }
        assert_eq!(playing.map(|p| p.seq), Some(5));
        r.fake.finish();
        until("finished", || r.fake.finished().len() == 1).await;
        r.shut().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_during_playback_or_with_a_queue_repeats_nothing() {
        let (_d, store) = store();
        live_on(&store, "default");
        let mut r = start(&store, FakeEngine::manual());
        r.connect("default", Some("T"), 4).await;
        r.item("default", msg(5, "Five.", Origin::Live));
        r.item("default", msg(6, "Six.", Origin::Live));
        until("five", || r.fake.speaking()).await;
        r.send(ManualEvent::Disconnect {
            ws: "default".into(),
        });
        let p = r.connect("default", Some("T"), 6).await;
        assert_eq!(p.watermarks.get("T"), Some(&5), "five started, six didn't");
        assert!(p.live);
        // The hub sends what came after the watermark again (and a confused one, five).
        r.item("default", msg(5, "Five.", Origin::Catchup));
        r.item("default", msg(6, "Six.", Origin::Catchup));
        r.item("default", msg(7, "Seven.", Origin::Live));
        for n in 1..=3 {
            until("a chunk", || r.fake.speaking()).await;
            r.fake.finish();
            until("done", || r.fake.finished().len() == n).await;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(r.fake.spoken(), vec!["Five.", "Six.", "Seven."]);
        assert_eq!(r.state().watermark("T"), Some(7));
        r.shut().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_restart_mid_message_never_repeats_it() {
        let (_d, store) = store();
        live_on(&store, "default");
        let mut r = start(&store, FakeEngine::manual());
        r.connect("default", Some("T"), 4).await;
        r.item("default", msg(5, "Five.", Origin::Live));
        until("five", || r.fake.speaking()).await;
        let fake = r.fake.clone();
        r.shut().await;
        assert_eq!(
            fake.events().last(),
            Some(&FakeEvent::Stopped("Five.".into()))
        );
        // A new speaker process: it resumes after five.
        let mut r = start(&store, FakeEngine::manual());
        let p = r.connect("default", Some("T"), 5).await;
        assert_eq!(p.watermarks.get("T"), Some(&5));
        r.shut().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_disconnect_before_the_first_message_loses_nothing() {
        let (_d, store) = store();
        let mut r = start(&store, FakeEngine::manual());
        r.connect("default", Some("T"), 10).await;
        r.send(ManualEvent::Disconnect {
            ws: "default".into(),
        });
        let p = r.connect("default", Some("T"), 12).await;
        assert_eq!(
            p.watermarks.get("T"),
            Some(&10),
            "resumes at the first head"
        );
        // A new orchestrator: read from its start, even after a disconnection.
        r.item("default", SpeechItem::Orchestrator { thread: "U".into() });
        until("U saved", || r.state().watermark("U") == Some(0)).await;
        r.send(ManualEvent::Disconnect {
            ws: "default".into(),
        });
        let p = r.connect("default", Some("U"), 3).await;
        assert_eq!(p.watermarks.get("U"), Some(&0));
        r.shut().await;
    }

    #[tokio::test(start_paused = true)]
    async fn of_two_requests_only_the_latest_batch_is_read_then_live_ones() {
        let (_d, store) = store();
        live_on(&store, "default");
        let mut r = start(&store, FakeEngine::timed(Duration::from_secs(1)));
        r.connect("default", Some("T"), 0).await;
        r.item("default", msg(1, "Live before.", Origin::Live));
        until("live", || r.fake.speaking()).await;
        r.item(
            "default",
            ctl(CtlOp::Replace, Some(json!({"batch": 1, "count": 2}))),
        );
        r.item("default", req(10, "First batch one.", 1));
        r.item(
            "default",
            ctl(CtlOp::Replace, Some(json!({"batch": 2, "count": 2}))),
        );
        r.item("default", req(11, "First batch two.", 1));
        r.item("default", req(20, "Second batch one.", 2));
        r.item("default", msg(30, "Live after.", Origin::Live));
        r.item("default", req(21, "Second batch two.", 2));
        until("all said", || r.fake.finished().len() >= 4).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        let spoken = r.fake.spoken();
        assert_eq!(spoken[0], "Live before.");
        // The first batch may have begun before the second replaced it.
        let rest: Vec<&str> = spoken[1..]
            .iter()
            .map(String::as_str)
            .filter(|s| !s.starts_with("First") && *s != "Reading 2 messages.")
            .collect();
        assert_eq!(
            rest,
            vec!["Second batch one.", "Second batch two.", "Live after."]
        );
        assert!(!spoken.iter().any(|s| s == "First batch two."));
        r.shut().await;
    }

    #[tokio::test(start_paused = true)]
    async fn missed_messages_are_announced_with_live_on() {
        let (_d, store) = store();
        live_on(&store, "default");
        let mut r = start(&store, FakeEngine::timed(Duration::from_secs(1)));
        r.connect("default", Some("T"), 0).await;
        r.item(
            "default",
            SpeechItem::Missed {
                thread: "T".into(),
                count: 3,
                upto: 9,
            },
        );
        until("said", || r.fake.finished().len() == 1).await;
        assert_eq!(
            r.fake.spoken(),
            vec!["3 earlier messages came while I was disconnected; they're on screen"]
        );
        assert_eq!(r.state().watermark("T"), Some(9));
        r.shut().await;
    }

    #[tokio::test(start_paused = true)]
    async fn voice_rate_and_live_are_saved_and_sent_on_the_next_attach() {
        let (_d, store) = store();
        let mut r = start(&store, FakeEngine::timed(Duration::from_secs(1)));
        r.connect("w1", None, 0).await;
        r.item("w1", ctl(CtlOp::Voice, Some(json!("Ava"))));
        r.item("w1", ctl(CtlOp::Rate, Some(json!(220))));
        r.item("w1", ctl(CtlOp::Live, Some(json!(true))));
        until("saved", || {
            let s = r.state();
            s.voice.as_deref() == Some("Ava") && s.rate == Some(220) && s.live("local", "w1")
        })
        .await;
        let p = r.connect("w2", None, 0).await;
        assert_eq!(p.voice.as_deref(), Some("Ava"));
        assert!(!p.live, "live is per workspace");
        r.item("w1", msg(1, "Hi.", Origin::Live));
        until("said", || r.fake.finished().len() == 1).await;
        match &r.fake.events()[0] {
            FakeEvent::Speak { prefs, .. } => {
                assert_eq!((prefs.voice.as_deref(), prefs.rate), (Some("Ava"), 220))
            }
            e => panic!("{e:?}"),
        }
        r.shut().await;
    }

    /// A save that fails (the state's directory unwritable, as a full disk) keeps its changes:
    /// in effect meanwhile (the next attach says live reading is on), saved once it can be.
    #[tokio::test(start_paused = true)]
    async fn a_failed_save_keeps_its_changes() {
        use std::os::unix::fs::PermissionsExt;
        let (d, store) = store();
        let mut r = start(&store, FakeEngine::timed(Duration::from_secs(1)));
        r.connect("w1", None, 0).await;
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        r.item("w1", ctl(CtlOp::Live, Some(json!(true))));
        until("said it failed", || {
            r.notes
                .lock()
                .unwrap()
                .iter()
                .any(|n| n.contains("saving its state"))
        })
        .await;
        let p = r.connect("w1", None, 0).await;
        assert!(p.live, "in effect though not saved");
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        r.item("w1", ctl(CtlOp::Rate, Some(json!(210))));
        until("saved", || {
            let s = r.state();
            s.live("local", "w1") && s.rate == Some(210)
        })
        .await;
        r.shut().await;
    }

    /// An attach's head is saved by the link before the next attach is made: a quick
    /// reconnect carries it, whenever the speaker gets to the first attach's event.
    #[tokio::test(start_paused = true)]
    async fn a_quick_reconnect_carries_the_first_attach_head() {
        let (_d, store) = store();
        let mut r = start(&store, FakeEngine::timed(Duration::from_secs(1)));
        let p = r.connect("w1", Some("O"), 10).await;
        assert!(!p.watermarks.contains_key("O"));
        let p = r.connect("w1", Some("O"), 12).await;
        assert_eq!(
            p.watermarks.get("O"),
            Some(&10),
            "from the first head, not the second"
        );
        r.item(
            "w1",
            SpeechItem::Orchestrator {
                thread: "O2".into(),
            },
        );
        let p = r.connect("w1", Some("O2"), 5).await;
        assert_eq!(
            p.watermarks.get("O2"),
            Some(&0),
            "a new orchestrator from its start"
        );
        r.shut().await;
    }

    /// An attach reads the state file afresh: what another speaker of this device saved
    /// meanwhile (it read on, live reading was switched on) is what the hub gets, and live
    /// reading follows it.
    #[tokio::test(start_paused = true)]
    async fn an_attach_reads_what_another_speaker_saved() {
        let (_d, store) = store();
        let mut r = start(&store, FakeEngine::timed(Duration::from_secs(1)));
        let p = r.connect("w1", None, 0).await;
        assert!(!p.live);
        // The other speaker, through the same file.
        store
            .update(|s| {
                s.live.insert(live_key("local", "w1"), true);
                s.set_watermark("t1", 41);
                s.voice = Some("Zoe".into());
                s.rate = Some(240);
            })
            .unwrap();
        let p = r.connect("w1", None, 0).await;
        assert!(p.live);
        assert_eq!(p.watermarks.get("t1"), Some(&41));
        assert_eq!(p.voice.as_deref(), Some("Zoe"));
        r.item("w1", msg(42, "Read now.", Origin::Live));
        until("said", || r.fake.finished().len() == 1).await;
        match &r.fake.events()[0] {
            FakeEvent::Speak { prefs, .. } => {
                assert_eq!((prefs.voice.as_deref(), prefs.rate), (Some("Zoe"), 240))
            }
            e => panic!("{e:?}"),
        }
        r.shut().await;
    }

    #[tokio::test(start_paused = true)]
    async fn each_hub_hears_only_what_it_plays_and_several_live_are_named() {
        let (_d, store) = store();
        live_on(&store, "a");
        live_on(&store, "b");
        let mut r = start(&store, FakeEngine::manual());
        r.connect("a", None, 0).await;
        r.connect("b", None, 0).await;
        r.item("a", msg(1, "From a.", Origin::Live));
        until("speaking", || r.fake.speaking()).await;
        assert_eq!(r.fake.spoken(), vec!["a:"]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (mut a, mut b) = (None, None);
        while let Ok((ws, s)) = r.hub.states.try_recv() {
            match ws.as_str() {
                "a" => a = Some(s),
                _ => b = Some(s),
            }
        }
        assert_eq!(a.unwrap().playing.map(|p| p.seq), Some(1));
        let b = b.unwrap();
        assert_eq!((b.playing, b.live), (None, true));
        r.shut().await;
    }

    /// A voice or rate no hub would send (too long, a control character, out of range) is
    /// neither used nor saved; a good one is.
    #[tokio::test(start_paused = true)]
    async fn a_bad_voice_or_rate_from_a_hub_is_ignored() {
        let (_d, store) = store();
        let mut r = start(&store, FakeEngine::timed(Duration::from_secs(1)));
        r.connect("w", None, 0).await;
        r.item("w", ctl(CtlOp::Voice, Some(json!("v".repeat(129)))));
        r.item("w", ctl(CtlOp::Voice, Some(json!("Ava\u{1b}[31m"))));
        r.item("w", ctl(CtlOp::Rate, Some(json!(79))));
        r.item("w", ctl(CtlOp::Rate, Some(json!(9000))));
        r.item("w", ctl(CtlOp::Rate, Some(json!(250))));
        until("the good rate saved", || r.state().rate == Some(250)).await;
        let s = r.state();
        assert_eq!(s.voice, None);
        let p = r.connect("w", None, 0).await;
        assert_eq!(p.voice, None);
        r.shut().await;
    }

    /// `--voice` (or a TUI's voice) a hub would refuse at every attach, or a rate out of
    /// range, is refused at start.
    #[tokio::test(start_paused = true)]
    async fn a_bad_voice_or_rate_is_refused_at_start() {
        let (_d, store) = store();
        let notes = Arc::new(Mutex::new(Vec::new()));
        for (voice, rate) in [
            (Some("v".repeat(129)), None),
            (Some("a\nb".into()), None),
            (None, Some(50)),
        ] {
            let (link, _hub) = manual();
            let (engine, _) = FakeEngine::manual();
            let o = SpeakerOpts {
                voice,
                rate,
                ..opts(&store, &notes)
            };
            let e = run(o, LinkSpec::Manual(link), Box::new(engine), None, async {})
                .await
                .unwrap_err();
            assert!(e.to_string().starts_with("speaker "), "{e}");
        }
    }

    /// A message longer than a hub sends is cut there, and said to be cut short.
    #[tokio::test(start_paused = true)]
    async fn a_message_longer_than_a_hub_sends_is_cut_short() {
        let (_d, store) = store();
        live_on(&store, "default");
        let mut r = start(&store, FakeEngine::timed(Duration::from_millis(1)));
        r.connect("default", None, 0).await;
        let text = "Word é. ".repeat(TEXT_MAX / 8 + 100);
        r.item("default", msg(1, &text, Origin::Live));
        until("cut short", || {
            r.fake
                .finished()
                .last()
                .is_some_and(|l| l == "message cut short; the rest is on screen.")
        })
        .await;
        let said: usize = r.fake.spoken().iter().map(String::len).sum();
        assert!(said < TEXT_MAX + 1000, "{said}");
        r.shut().await;
    }

    /// A batch some of whose messages never come (a newer hub's, which this speaker can't
    /// read) holds live reading for a few seconds only.
    #[tokio::test(start_paused = true)]
    async fn a_batch_missing_messages_holds_live_reading_only_a_while() {
        let (_d, store) = store();
        live_on(&store, "default");
        let mut r = start(&store, FakeEngine::timed(Duration::from_millis(100)));
        r.connect("default", Some("T"), 0).await;
        r.item(
            "default",
            ctl(CtlOp::Replace, Some(json!({"batch": 1, "count": 2}))),
        );
        r.item("default", req(10, "Asked for.", 1));
        r.item("default", msg(30, "Live after.", Origin::Live));
        until("asked for", || {
            r.fake.finished().contains(&"Asked for.".to_string())
        })
        .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            !r.fake.spoken().contains(&"Live after.".to_string()),
            "held"
        );
        until("live after", || {
            r.fake.finished().contains(&"Live after.".to_string())
        })
        .await;
        r.shut().await;
    }

    /// What a round changed is saved before the engine is waited on: a speaker aborted while
    /// the engine silences itself (the TUI's, on quit) has saved it already.
    #[tokio::test(start_paused = true)]
    async fn changes_are_saved_before_the_engine_is_waited_on() {
        let (_d, store) = store();
        live_on(&store, "default");
        let (engine, fake) = FakeEngine::manual();
        let mut r = start(
            &store,
            (engine.with_slow_stop(Duration::from_secs(60)), fake),
        );
        r.connect("default", Some("T"), 0).await;
        r.item("default", msg(1, "One.", Origin::Live));
        until("speaking", || r.fake.speaking()).await;
        r.item("default", msg(2, "Two.", Origin::Live));
        // Live off: the chunk stops, and the engine takes a minute to go quiet.
        r.item("default", ctl(CtlOp::Live, Some(json!(false))));
        until("stopped", || !r.fake.speaking()).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let s = r.state();
        assert!(!s.live("local", "default"), "live off saved");
        assert_eq!(s.watermark("T"), Some(2), "two, dropped, saved as handled");
        r.task.abort();
    }

    /// A message that starts after a skip counts as read only once the engine says it: while
    /// the engine still goes quiet (a slow `spd-say -C`), a speaker ended would read it again.
    #[tokio::test(start_paused = true)]
    async fn a_start_is_saved_once_the_engine_says_it() {
        let (_d, store) = store();
        live_on(&store, "default");
        let (engine, fake) = FakeEngine::manual();
        let mut r = start(
            &store,
            (engine.with_slow_stop(Duration::from_secs(60)), fake),
        );
        r.connect("default", Some("T"), 0).await;
        r.item("default", msg(1, "One.", Origin::Live));
        until("speaking", || r.fake.speaking()).await;
        r.item("default", msg(2, "Two.", Origin::Live));
        tokio::time::sleep(Duration::from_secs(1)).await;
        r.keys.send(Key::Skip).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            r.state().watermark("T"),
            Some(1),
            "two not saved while the engine stops"
        );
        tokio::time::sleep(Duration::from_secs(60)).await;
        until("two said", || r.fake.speaking()).await;
        until("two saved", || r.state().watermark("T") == Some(2)).await;
        r.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn keys_stop_and_quit() {
        let (_d, store) = store();
        live_on(&store, "default");
        let mut r = start(&store, FakeEngine::manual());
        r.connect("default", None, 0).await;
        r.item("default", msg(1, "One.", Origin::Live));
        r.item("default", msg(2, "Two.", Origin::Live));
        until("speaking", || r.fake.speaking()).await;
        r.keys.send(Key::Stop).await.unwrap();
        until("stopped", || !r.fake.speaking()).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(r.fake.spoken(), vec!["One."]);
        r.keys.send(Key::Quit).await.unwrap();
        r.task.await.unwrap().unwrap();
        assert!(r
            .notes
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.contains("connected")));
    }
}
