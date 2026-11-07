//! A phone's own speaker (`docs/mobile.md`, Voice). A phone can't run `twr speak`, so the
//! gateway, on the hub's machine, runs a speaker on its behalf: [`twr_speech::run`] on this
//! machine's hub of the device's workspace, with a [`DeviceEngine`] whose voice is the phone's.
//! Each utterance goes to the phone over its authenticated socket (`voice-speak`); the phone
//! says it with the platform's voice and acks it (`voice-speaker` `said`), which ends the
//! engine's future. A future dropped early (stop, skip, a pause) tells the phone to stop
//! (`voice-speak-stop`).
//!
//! - **Controls** come from the phone as `voice-speaker` calls and go to the speaker as
//!   [`Key`]s, here: pausing the voice the moment the user talks never waits for the hub.
//! - **State** ([`twr_speech::SpeakerOpts::observe`]) goes to the phone as
//!   `voice-device-speaker` events.
//! - **Its own state file** per device (`mobile/speakers/<device>/speech-state.json` in the
//!   root's data directory): its device id, watermarks, live switch and ownership lock are the
//!   phone's. The hub keeps one speaker per device id, and [`StateStore::try_own`] locks a file
//!   beside the state file, so the hub machine's own speaker (a TUI's, `twr speak`) and the
//!   phone's never stand by for each other: both speak.
//! - **Lifetime.** Starting it turns live reading on for that device and workspace (its
//!   state's live switch, before it attaches). It ends when the phone stops it, switches
//!   workspace, has had no socket for [`SOCKET_GRACE`], or is revoked or loses `read`.
//!
//! Security: the hub refuses `speech.attach` on a device's own connection (no device scope
//! names it), so the speaker attaches as this gateway's user, like `twr speak` on this
//! machine. What it can carry is only what the hub's speech feed sends a speaker (the
//! orchestrator's finished messages, `/read N`'s), which the device's `read` scope can see in
//! full anyway: listening is reading. The device sends controls and acks only; no call takes
//! text, so a phone can never make the hub (or another speaker) say anything. Acks resolve
//! only this device's own pending utterances.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};
use twr_core::config::SpeechConfig;
use twr_core::speech::SpeechStateParams;
use twr_speech::engine::{Engine, Prefs, SpeakFuture};
use twr_speech::state::{live_key, LOCAL_HOST};
use twr_speech::{Key, LinkSpec, SpeakerOpts, StateStore};

use super::auth::Device;

/// How long a phone may have no socket before its speaker ends ("disconnected for good"). A
/// reconnect within it (a network change, a short trip to the background) keeps it.
pub const SOCKET_GRACE: Duration = Duration::from_secs(60);

/// How long an utterance waits for a socket before it fails.
pub const SOCKET_WAIT: Duration = Duration::from_secs(30);

/// How often the speaker's watch checks revocation and the sockets.
const WATCH_EVERY: Duration = Duration::from_secs(2);

/// The longest an utterance may take on the phone before it counts as failed: generous for
/// the slowest rate (80 words per minute) of the longest chunk.
pub fn utterance_budget(text: &str) -> Duration {
    let chars = text.chars().count() as u64;
    Duration::from_secs(15 + chars / 3).min(Duration::from_secs(180))
}

/// A `voice-speaker` call.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Start the phone's speaker for its current workspace (live reading on).
    Start,
    /// End it.
    Stop,
    /// Pause it (the user started to talk).
    Pause,
    /// Go on after a pause.
    Resume,
    /// End the message being read.
    Skip,
    /// Stop now and empty the queue (live reading stays on).
    Silence,
    /// The phone said utterance `id` to its end.
    Said(u64),
    /// The phone could not say utterance `id`: why, one line.
    Failed(u64, String),
}

impl Op {
    /// The call's params as an op. Nothing else is taken: no op carries text to say.
    pub fn parse(params: &Value) -> Result<Op> {
        let id = || params["id"].as_u64().context("Utterance id required");
        Ok(match params["op"].as_str().unwrap_or("") {
            "start" => Op::Start,
            "stop" => Op::Stop,
            "pause" => Op::Pause,
            "resume" => Op::Resume,
            "skip" => Op::Skip,
            "silence" => Op::Silence,
            "said" => Op::Said(id()?),
            "failed" => {
                let why: String = params["error"]
                    .as_str()
                    .unwrap_or("the phone could not say it")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(200)
                    .collect();
                Op::Failed(id()?, why)
            }
            _ => bail!("Unknown speaker operation"),
        })
    }

    /// The speaker key of a control, if it is one.
    pub fn key(&self) -> Option<Key> {
        Some(match self {
            Op::Pause => Key::Pause,
            Op::Resume => Key::Resume,
            Op::Skip => Key::Skip,
            Op::Silence => Key::Stop,
            _ => return None,
        })
    }
}

/// Whether `device` may run a speaker: it needs `read` (listening is reading).
pub fn allowed(device: &Device) -> Result<()> {
    anyhow::ensure!(
        device.scopes.iter().any(|s| s == "read"),
        "Reading aloud on this device requires read permission"
    );
    Ok(())
}

/// The phone's speaker name in `/read status`: its device name and "phone", 1-64 printable
/// characters.
pub fn speaker_name(device: &str) -> String {
    let clean: String = device
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(56)
        .collect();
    let name = if clean.is_empty() {
        "phone".to_owned()
    } else if clean.to_lowercase().ends_with("phone") {
        clean
    } else {
        format!("{clean} phone")
    };
    if twr_core::speech::name_error("name", &name, 64).is_some() {
        "phone".to_owned()
    } else {
        name
    }
}

/// The state file of `device`'s speaker, under the gateway's data directory `mobile`.
pub fn state_store(mobile: &Path, device: &str) -> Result<StateStore> {
    anyhow::ensure!(
        !device.is_empty()
            && device.len() <= 64
            && device
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid device id"
    );
    Ok(StateStore::in_data_dir(
        &mobile.join("speakers").join(device),
    ))
}

/// What the gateway shares with a phone's engine: the phone's event stream, how many of its
/// sockets are open, and the utterances waiting for its ack.
pub struct Outbox {
    events: mpsc::Sender<Value>,
    sockets: watch::Sender<usize>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>,
    next: AtomicU64,
}

impl Outbox {
    pub fn new(events: mpsc::Sender<Value>) -> Arc<Outbox> {
        Arc::new(Outbox {
            events,
            sockets: watch::channel(0).0,
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
        })
    }

    /// A socket of the phone is open while the guard lives.
    pub fn socket(self: &Arc<Self>) -> SocketGuard {
        self.sockets.send_modify(|n| *n += 1);
        SocketGuard(self.clone())
    }

    /// The phone's ack of utterance `id`; whether one was waiting (a late or foreign id is
    /// ignored).
    pub fn ack(&self, id: u64, result: Result<(), String>) -> bool {
        let waiting = self.pending.lock().unwrap().remove(&id);
        waiting.is_some_and(|tx| tx.send(result).is_ok())
    }

    fn send(&self, event: Value) {
        // Never blocks the speaker: a phone whose stream is full is told nothing more until
        // it drains (its utterance then times out).
        let _ = self.events.try_send(event);
    }
}

/// An open socket of the phone ([`Outbox::socket`]).
pub struct SocketGuard(Arc<Outbox>);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.0.sockets.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// The phone's voice, as a speaker's engine (see the module documentation).
pub struct DeviceEngine {
    out: Arc<Outbox>,
}

impl DeviceEngine {
    pub fn new(out: Arc<Outbox>) -> DeviceEngine {
        DeviceEngine { out }
    }
}

/// An utterance on its way: dropped before the phone acked it, the phone is told to stop.
struct Utterance {
    out: Arc<Outbox>,
    id: u64,
    sent: bool,
    done: bool,
}

impl Drop for Utterance {
    fn drop(&mut self) {
        self.out.pending.lock().unwrap().remove(&self.id);
        if self.sent && !self.done {
            self.out
                .send(json!({"type":"voice-speak-stop","id":self.id}));
        }
    }
}

impl Engine for DeviceEngine {
    fn name(&self) -> &str {
        "phone"
    }

    /// Sends `chunk` to the phone and waits for its ack. Without a socket it waits up to
    /// [`SOCKET_WAIT`] for one; a socket lost mid-utterance has it said again on the next.
    fn speak(&mut self, chunk: &str, prefs: &Prefs) -> SpeakFuture {
        let out = self.out.clone();
        let text = chunk.to_owned();
        let (voice, rate) = (prefs.voice.clone(), prefs.rate);
        Box::pin(async move {
            let id = out.next.fetch_add(1, Ordering::SeqCst) + 1;
            let (tx, mut rx) = oneshot::channel();
            out.pending.lock().unwrap().insert(id, tx);
            let mut utterance = Utterance {
                out: out.clone(),
                id,
                sent: false,
                done: false,
            };
            let mut sockets = out.sockets.subscribe();
            let deadline = tokio::time::Instant::now() + utterance_budget(&text);
            loop {
                let open = tokio::time::timeout(SOCKET_WAIT, sockets.wait_for(|n| *n > 0))
                    .await
                    .map(|r| r.is_ok());
                if open != Ok(true) {
                    bail!("the phone is not connected");
                }
                out.events
                    .send(
                        json!({"type":"voice-speak","id":id,"text":text,"voice":voice,"rate":rate}),
                    )
                    .await
                    .map_err(|_| anyhow!("the phone's connection closed"))?;
                utterance.sent = true;
                tokio::select! {
                    result = &mut rx => {
                        utterance.done = true;
                        return match result {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(why)) => Err(anyhow!("phone voice: {why}")),
                            Err(_) => Err(anyhow!("the speaker stopped")),
                        };
                    }
                    lost = sockets.wait_for(|n| *n == 0) => {
                        if lost.is_err() {
                            bail!("the phone's connection closed");
                        }
                        // Said again, whole, once the phone is back.
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        bail!("the phone did not finish saying it");
                    }
                }
            }
        })
    }
}

/// What the phone is told about its speaker (`voice-device-speaker`).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct Status {
    pub running: bool,
    pub playing: bool,
    pub paused: bool,
    pub queued: u32,
    pub live: bool,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Status {
    fn event(&self) -> Value {
        json!({"type":"voice-device-speaker","speaker":self})
    }
}

/// How to start a phone's speaker.
pub struct Start {
    pub workspace: String,
    pub name: String,
    pub store: StateStore,
    pub link: LinkSpec,
    pub config: SpeechConfig,
    /// Whether the device may still listen (not revoked, still `read`).
    pub allowed: Arc<dyn Fn() -> bool + Send + Sync>,
}

struct Running {
    id: u64,
    workspace: String,
    keys: mpsc::Sender<Key>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

/// A device's speaker, if it runs, and what the phone was last told.
pub struct DeviceVoice {
    out: Arc<Outbox>,
    running: Arc<Mutex<Option<Running>>>,
    status: Arc<Mutex<Status>>,
    starts: AtomicU64,
    /// Held across a start or a stop: two of the phone's calls run at once, and a start that
    /// replaced another's speaker would otherwise leave the first one's end publishing
    /// "stopped" over the running one.
    changing: tokio::sync::Mutex<()>,
}

impl DeviceVoice {
    pub fn new(events: mpsc::Sender<Value>) -> DeviceVoice {
        DeviceVoice {
            out: Outbox::new(events),
            running: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new(Status::default())),
            starts: AtomicU64::new(0),
            changing: tokio::sync::Mutex::new(()),
        }
    }

    /// A socket of the phone is open while the guard lives.
    pub fn socket(&self) -> SocketGuard {
        self.out.socket()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// Whether a speaker runs, and for which workspace.
    pub fn workspace(&self) -> Option<String> {
        self.running
            .lock()
            .unwrap()
            .as_ref()
            .filter(|r| !r.task.is_finished())
            .map(|r| r.workspace.clone())
    }

    /// Does `op`; `start` makes the speaker's [`Start`] only when it starts one. The status
    /// the phone was last told.
    pub async fn control(&self, op: Op, start: impl FnOnce() -> Result<Start>) -> Result<Value> {
        match op {
            Op::Said(id) => return Ok(json!({"acked": self.out.ack(id, Ok(()))})),
            Op::Failed(id, why) => return Ok(json!({"acked": self.out.ack(id, Err(why))})),
            Op::Start => {
                let _changing = self.changing.lock().await;
                let start = start()?;
                if self.workspace().as_deref() == Some(start.workspace.as_str()) {
                    return Ok(json!(self.status()));
                }
                self.stop().await;
                self.start(start)?;
            }
            Op::Stop => {
                let _changing = self.changing.lock().await;
                self.stop().await
            }
            op => {
                let key = op.key().expect("a control");
                let keys = self
                    .running
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|r| !r.task.is_finished())
                    .map(|r| r.keys.clone())
                    .context("This phone's speaker is not running")?;
                keys.send(key)
                    .await
                    .map_err(|_| anyhow!("This phone's speaker stopped"))?;
            }
        }
        Ok(json!(self.status()))
    }

    /// The phone switched to `workspace`: a speaker for another one ends (the phone starts
    /// one again for the new one).
    pub async fn workspace_changed(&self, workspace: &str) {
        if self.workspace().is_some_and(|w| w != workspace) {
            self.stop().await;
        }
    }

    fn publish(status: &Mutex<Status>, out: &Outbox, change: impl FnOnce(&mut Status)) {
        let mut s = status.lock().unwrap();
        let before = s.clone();
        change(&mut s);
        if *s != before {
            out.send(s.event());
        }
    }

    fn start(&self, start: Start) -> Result<()> {
        // Live reading on for this device and workspace, saved before it attaches: the attach
        // carries it.
        start
            .store
            .update(|s| {
                s.live.insert(live_key(LOCAL_HOST, &start.workspace), true);
            })
            .context("Saving the phone speaker's state")?;
        let id = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        let (keys, key_rx) = mpsc::channel(16);
        let (stop, stopped) = oneshot::channel::<()>();
        let observe: twr_speech::StateObserver = {
            let (status, out) = (self.status.clone(), self.out.clone());
            Arc::new(
                move |_ws: &str, state: &SpeechStateParams, speaking: bool| {
                    Self::publish(&status, &out, |s| {
                        // An announcement being said counts: the user talking over it pauses it.
                        s.playing = state.playing.is_some() || (speaking && !state.paused);
                        s.paused = state.paused;
                        s.queued = state.queued;
                        s.live = state.live;
                        s.error = state.error.clone();
                    })
                },
            )
        };
        let opts = SpeakerOpts {
            name: start.name.clone(),
            config: start.config,
            voice: None,
            rate: None,
            state: start.store,
            report: Some(Arc::new(|note: &str| {
                crate::gateway_log!("phone speaker: {note}")
            })),
            tty_keys_once_attached: false,
            observe: Some(observe),
        };
        Self::publish(&self.status, &self.out, |s| {
            *s = Status {
                running: true,
                name: start.name,
                workspace: Some(start.workspace.clone()),
                ..Status::default()
            }
        });
        let (status, out, running) = (self.status.clone(), self.out.clone(), self.running.clone());
        let mut sockets = self.out.sockets.subscribe();
        let allowed = start.allowed;
        let shutdown = async move {
            let mut stopped = stopped;
            let mut tick = tokio::time::interval(WATCH_EVERY);
            let mut alone: Option<tokio::time::Instant> = None;
            loop {
                tokio::select! {
                    _ = &mut stopped => return,
                    _ = tick.tick() => {}
                    _ = sockets.changed() => {}
                }
                if !allowed() {
                    return;
                }
                if *sockets.borrow() == 0 {
                    let since = *alone.get_or_insert_with(tokio::time::Instant::now);
                    if since.elapsed() >= SOCKET_GRACE {
                        return;
                    }
                } else {
                    alone = None;
                }
            }
        };
        let engine = Box::new(DeviceEngine::new(self.out.clone()));
        let task = tokio::spawn(async move {
            let result = twr_speech::run(opts, start.link, engine, Some(key_rx), shutdown).await;
            Self::publish(&status, &out, |s| {
                s.running = false;
                s.playing = false;
                s.paused = false;
                s.queued = 0;
                s.error = result.err().map(|e| format!("{e:#}"));
            });
            let mut running = running.lock().unwrap();
            if running.as_ref().is_some_and(|r| r.id == id) {
                *running = None;
            }
        });
        *self.running.lock().unwrap() = Some(Running {
            id,
            workspace: start.workspace,
            keys,
            stop: Some(stop),
            task,
        });
        Ok(())
    }

    /// Ends the speaker, if it runs, and waits for it (a few seconds at most).
    pub async fn stop(&self) {
        let Some(mut running) = self.running.lock().unwrap().take() else {
            return;
        };
        if let Some(stop) = running.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(Duration::from_secs(5), &mut running.task)
            .await
            .is_err()
        {
            running.task.abort();
        }
        Self::publish(&self.status, &self.out, |s| {
            s.running = false;
            s.playing = false;
            s.paused = false;
            s.queued = 0;
        });
    }
}

impl Drop for DeviceVoice {
    fn drop(&mut self) {
        if let Some(mut running) = self.running.lock().unwrap().take() {
            if let Some(stop) = running.stop.take() {
                let _ = stop.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::speech::{MsgKind, Origin, SpeechAttachAck, SpeechItem};
    use twr_speech::link::{manual, ManualEvent, ManualHub};

    fn device(scopes: &[&str]) -> Device {
        Device {
            id: "phone-1".into(),
            name: "Nacho's iPhone".into(),
            created: 0,
            token_hash: String::new(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    async fn next_event(rx: &mut mpsc::Receiver<Value>, kind: &str) -> Value {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("an event in time")
                .expect("the stream is open");
            if event["type"] == kind {
                return event;
            }
        }
    }

    #[test]
    fn listening_needs_read_and_ops_carry_no_text() {
        assert!(allowed(&device(&["read"])).is_ok());
        assert!(allowed(&device(&["read", "operate"])).is_ok());
        assert!(allowed(&device(&[])).is_err());
        assert!(allowed(&device(&["operate"])).is_err());
        for (op, want) in [
            ("start", Op::Start),
            ("stop", Op::Stop),
            ("pause", Op::Pause),
            ("resume", Op::Resume),
            ("skip", Op::Skip),
            ("silence", Op::Silence),
        ] {
            assert_eq!(
                Op::parse(&json!({"op":op,"text":"say this"})).unwrap(),
                want
            );
        }
        for bad in ["say", "speak", "", "quit"] {
            assert!(
                Op::parse(&json!({"op":bad,"text":"hello"})).is_err(),
                "{bad}"
            );
        }
        assert!(Op::parse(&json!({"op":"said"})).is_err());
        assert_eq!(
            Op::parse(&json!({"op":"failed","id":3,"error":"no\nvoice"})).unwrap(),
            Op::Failed(3, "novoice".into())
        );
    }

    /// Speaker calls are no mutations: they never claim a durable receipt, and a retried one
    /// can only repeat a control.
    #[test]
    fn speaker_calls_claim_no_receipts() {
        for op in ["start", "stop", "pause", "said"] {
            assert!(!super::super::receipts::mutation(
                "voice-speaker",
                &json!({"op":op,"id":1})
            ));
        }
    }

    #[test]
    fn controls_are_speaker_keys() {
        assert_eq!(Op::Pause.key(), Some(Key::Pause));
        assert_eq!(Op::Resume.key(), Some(Key::Resume));
        assert_eq!(Op::Skip.key(), Some(Key::Skip));
        assert_eq!(Op::Silence.key(), Some(Key::Stop));
        assert_eq!(Op::Start.key(), None);
        assert_eq!(Op::Stop.key(), None);
        assert_eq!(Op::Said(1).key(), None);
    }

    #[test]
    fn speaker_names_are_printable_and_short() {
        assert_eq!(speaker_name("Pixel 9"), "Pixel 9 phone");
        assert_eq!(speaker_name("Nacho's iPhone"), "Nacho's iPhone");
        assert_eq!(speaker_name("  \u{7}  "), "phone");
        let long = speaker_name(&"x".repeat(80));
        assert!(long.chars().count() <= 64);
        assert!(twr_core::speech::name_error("name", &long, 64).is_none());
        assert!(state_store(Path::new("/tmp"), "../escape").is_err());
        assert!(state_store(Path::new("/tmp"), "").is_err());
    }

    /// The phone's speaker and the hub machine's own one never stand by for each other: each
    /// has its own device id and its own ownership lock.
    #[test]
    fn a_phone_speaker_and_this_machines_speaker_both_speak() {
        let dir = tempfile::tempdir().unwrap();
        let machine = StateStore::in_data_dir(dir.path());
        let phone = state_store(&dir.path().join("mobile"), "phone-1").unwrap();
        let other = state_store(&dir.path().join("mobile"), "phone-2").unwrap();
        let _a = machine.try_own(LOCAL_HOST).unwrap().expect("the machine's");
        let _b = phone.try_own(LOCAL_HOST).unwrap().expect("the phone's too");
        let _c = other
            .try_own(LOCAL_HOST)
            .unwrap()
            .expect("another phone's too");
        let ids = [
            machine.load().unwrap().device,
            phone.load().unwrap().device,
            other.load().unwrap().device,
        ];
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[1], ids[2]);
        assert_ne!(ids[0], ids[2]);
        // A second speaker of the same phone does stand by.
        let again = state_store(&dir.path().join("mobile"), "phone-1").unwrap();
        assert!(again.try_own(LOCAL_HOST).unwrap().is_none());
    }

    #[tokio::test]
    async fn speaking_sends_the_text_and_the_ack_ends_it() {
        let (tx, mut rx) = mpsc::channel(16);
        let out = Outbox::new(tx);
        let _socket = out.socket();
        let mut engine = DeviceEngine::new(out.clone());
        let prefs = Prefs {
            voice: Some("Ava".into()),
            rate: 180,
            volume: 1.0,
        };
        let said = tokio::spawn(engine.speak("Hello there.", &prefs));
        let event = next_event(&mut rx, "voice-speak").await;
        assert_eq!(event["text"], "Hello there.");
        assert_eq!(event["voice"], "Ava");
        assert_eq!(event["rate"], 180);
        let id = event["id"].as_u64().unwrap();
        assert!(!out.ack(id + 100, Ok(())), "a foreign id is ignored");
        assert!(out.ack(id, Ok(())));
        said.await.unwrap().unwrap();
        assert!(!out.ack(id, Ok(())), "acked once");
        // A failure is the engine's error.
        let failed = tokio::spawn(engine.speak("Again.", &prefs));
        let id = next_event(&mut rx, "voice-speak").await["id"]
            .as_u64()
            .unwrap();
        out.ack(id, Err("no voice installed".into()));
        let e = failed.await.unwrap().unwrap_err();
        assert!(format!("{e:#}").contains("no voice installed"));
        // Ended normally: the phone is not told to stop.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_dropped_utterance_tells_the_phone_to_stop() {
        let (tx, mut rx) = mpsc::channel(16);
        let out = Outbox::new(tx);
        let _socket = out.socket();
        let mut engine = DeviceEngine::new(out.clone());
        let speaking = tokio::spawn(engine.speak("A long sentence.", &Prefs::default()));
        let id = next_event(&mut rx, "voice-speak").await["id"]
            .as_u64()
            .unwrap();
        speaking.abort();
        let stop = next_event(&mut rx, "voice-speak-stop").await;
        assert_eq!(stop["id"], id);
        assert!(!out.ack(id, Ok(())), "nothing waits for it any more");
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_socket_it_waits_then_fails_and_a_lost_socket_says_it_again() {
        let (tx, mut rx) = mpsc::channel(16);
        let out = Outbox::new(tx);
        let mut engine = DeviceEngine::new(out.clone());
        let e = engine
            .speak("Nobody hears.", &Prefs::default())
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("not connected"));
        assert!(rx.try_recv().is_err(), "nothing was sent");

        let socket = out.socket();
        let speaking = tokio::spawn(engine.speak("Said twice.", &Prefs::default()));
        let first = next_event(&mut rx, "voice-speak").await["id"]
            .as_u64()
            .unwrap();
        drop(socket);
        tokio::task::yield_now().await;
        let _back = out.socket();
        let again = next_event(&mut rx, "voice-speak").await;
        assert_eq!(again["id"].as_u64(), Some(first));
        assert_eq!(again["text"], "Said twice.");
        out.ack(first, Ok(()));
        speaking.await.unwrap().unwrap();
    }

    struct Rig {
        voice: DeviceVoice,
        events: mpsc::Receiver<Value>,
        hub: Option<ManualHub>,
        store: StateStore,
        _dir: tempfile::TempDir,
    }

    fn rig() -> Rig {
        let (tx, events) = mpsc::channel(64);
        let dir = tempfile::tempdir().unwrap();
        let store = state_store(dir.path(), "phone-1").unwrap();
        Rig {
            voice: DeviceVoice::new(tx),
            events,
            hub: None,
            store,
            _dir: dir,
        }
    }

    impl Rig {
        async fn start(&mut self, allowed: Arc<dyn Fn() -> bool + Send + Sync>) -> Value {
            let (link, hub) = manual();
            self.hub = Some(hub);
            let store = self.store.clone();
            self.voice
                .control(Op::Start, || {
                    Ok(Start {
                        workspace: "work".into(),
                        name: speaker_name("Pixel"),
                        store,
                        link: LinkSpec::Manual(link),
                        config: SpeechConfig::default(),
                        allowed,
                    })
                })
                .await
                .unwrap()
        }
        fn hub(&self) -> &ManualHub {
            self.hub.as_ref().unwrap()
        }
    }

    fn msg(seq: u64, text: &str) -> SpeechItem {
        SpeechItem::Msg {
            thread: "T".into(),
            seq,
            turn: Some(1),
            ts: "2026-11-07T12:00:00Z".into(),
            kind: MsgKind::Reply,
            text: text.into(),
            truncated: false,
            origin: Origin::Live,
            batch: None,
        }
    }

    /// The whole speaker: started live, a hub message reaches the phone as an utterance, the
    /// phone's controls are keys (a pause stops the utterance on the phone, resume says it
    /// again), state goes to the phone, and stop ends it.
    #[tokio::test]
    async fn the_phone_speaker_reads_the_feed_and_takes_controls() {
        let mut rig = rig();
        let _socket = rig.voice.socket();
        let status = rig.start(Arc::new(|| true)).await;
        assert_eq!(status["running"], true);
        assert_eq!(status["name"], "Pixel phone");
        assert!(rig.store.load().unwrap().live(LOCAL_HOST, "work"));
        let mut hub = rig.hub.take().unwrap();
        hub.events
            .send(ManualEvent::Connect {
                ws: "work".into(),
                ack: SpeechAttachAck {
                    orchestrator: Some("T".into()),
                    head: 1,
                },
            })
            .unwrap();
        let (ws, params) = hub.attaches.recv().await.unwrap();
        assert_eq!(ws, "work");
        assert!(params.live, "the attach turns live reading on");
        assert_eq!(params.name, "Pixel phone");
        assert_eq!(params.engine, "phone");
        hub.events
            .send(ManualEvent::Item {
                ws: "work".into(),
                item: msg(2, "The tests pass."),
            })
            .unwrap();
        let speak = next_event(&mut rig.events, "voice-speak").await;
        assert_eq!(speak["text"], "The tests pass.");
        let id = speak["id"].as_u64().unwrap();
        // The state the phone hears: playing (told before or after the utterance).
        assert!(rig.voice.status().playing);
        // Pause: a key, no hub round trip; the utterance stops on the phone.
        rig.voice
            .control(Op::Pause, || unreachable!())
            .await
            .unwrap();
        let stop = next_event(&mut rig.events, "voice-speak-stop").await;
        assert_eq!(stop["id"], id);
        // Resume: said again.
        rig.voice
            .control(Op::Resume, || unreachable!())
            .await
            .unwrap();
        let again = next_event(&mut rig.events, "voice-speak").await;
        assert_eq!(again["text"], "The tests pass.");
        let id = again["id"].as_u64().unwrap();
        rig.voice
            .control(Op::Said(id), || unreachable!())
            .await
            .unwrap();
        loop {
            let s = next_event(&mut rig.events, "voice-device-speaker").await;
            if s["speaker"]["playing"] == false && s["speaker"]["running"] == true {
                break;
            }
        }
        // The hub was told what it plays, as from any speaker.
        assert!(hub.states.try_recv().is_ok());
        // Silence empties the queue (a key too).
        rig.voice
            .control(Op::Silence, || unreachable!())
            .await
            .unwrap();
        // A second start for the same workspace keeps the one running.
        let again = rig.start(Arc::new(|| true)).await;
        assert_eq!(again["running"], true);
        assert!(rig.voice.workspace().is_some());
        rig.voice
            .control(Op::Stop, || unreachable!())
            .await
            .unwrap();
        assert!(!rig.voice.status().running);
        assert!(rig.voice.workspace().is_none());
        let e = rig.voice.control(Op::Pause, || unreachable!()).await;
        assert!(e.is_err(), "no speaker, no controls");
    }

    #[tokio::test]
    async fn a_revoked_phone_or_another_workspace_ends_the_speaker() {
        let mut rig = rig();
        let _socket = rig.voice.socket();
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let check = revoked.clone();
        rig.start(Arc::new(move || !check.load(Ordering::SeqCst)))
            .await;
        revoked.store(true, Ordering::SeqCst);
        loop {
            let s = next_event(&mut rig.events, "voice-device-speaker").await;
            if s["speaker"]["running"] == false {
                break;
            }
        }
        assert!(rig.voice.workspace().is_none());

        rig.start(Arc::new(|| true)).await;
        rig.voice.workspace_changed("work").await;
        assert_eq!(rig.voice.workspace().as_deref(), Some("work"));
        rig.voice.workspace_changed("other").await;
        assert!(rig.voice.workspace().is_none());
        let _ = rig.hub();
    }

    #[tokio::test(start_paused = true)]
    async fn a_phone_gone_for_good_ends_the_speaker() {
        let mut rig = rig();
        let socket = rig.voice.socket();
        rig.start(Arc::new(|| true)).await;
        drop(socket);
        tokio::time::sleep(SOCKET_GRACE / 2).await;
        assert!(rig.voice.workspace().is_some(), "a short absence is fine");
        let back = rig.voice.socket();
        tokio::time::sleep(SOCKET_GRACE).await;
        assert!(rig.voice.workspace().is_some(), "it came back");
        drop(back);
        tokio::time::sleep(SOCKET_GRACE + WATCH_EVERY * 2).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(rig.voice.workspace().is_none(), "gone for good");
    }
}
