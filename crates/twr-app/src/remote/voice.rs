//! A phone's own speaker (`docs/mobile.md`, Voice). A phone can't run `twr speak`, so the
//! gateway, on the hub's machine, runs a speaker on its behalf: [`twr_speech::run`] on this
//! machine's hub of the device's workspace, with a [`DeviceEngine`] whose voice is the phone's.
//! Each utterance goes to the phone over its authenticated socket (`voice-speak`); the phone
//! says it with the platform's voice and acks it (`voice-speaker` `said`), which ends the
//! engine's future. A future dropped early (stop, skip, a pause) tells the phone to stop
//! (`voice-speak-stop`).
//!
//! - **Voices.** A voice per language is the phone's own choice ([`select`], kept in its state
//!   file, never the hub machine's `[speech]` voices): one of the phone's voices (an
//!   identifier without `/`), said by the phone from the text as above, the message's
//!   language with it (`lang`); or one of twr's natural voices (`kokoro/af_heart`), made here
//!   by `twr voices render` ([`Renderer`]) and sent to the phone as audio (`voice-audio`:
//!   16-bit mono PCM pieces, paced to how fast they play), which plays it and acks it the
//!   same way. Nothing plays on this machine.
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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
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

/// The languages a phone picks a voice for (`twr_core::speech::LANGUAGES`).
pub fn language_ok(lang: &str) -> bool {
    twr_core::speech::LANGUAGES.contains(&lang)
}

/// Whether `voice` is one of twr's natural voices (`<model>/<voice>`), which this machine
/// makes for the phone, rather than one of the phone's own (an iOS identifier such as
/// `com.apple.voice.premium.en-US.Ava`, an Android name such as `es-us-x-esd-local`: neither
/// has a `/`).
pub fn is_natural(voice: &str) -> bool {
    voice.split_once('/').is_some_and(|(model, name)| {
        !model.is_empty()
            && !name.is_empty()
            && model
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

/// A sentence in each language, said by a voice the phone previews: the gateway's own words
/// (no call takes text to say).
pub fn preview_text(lang: &str) -> &'static str {
    match lang {
        "es" => "Hola. Así sueno cuando leo tus respuestas en voz alta.",
        _ => "Hi. This is how I sound when I read your replies aloud.",
    }
}

/// twr's natural voices on this machine, as `twr voices list --json` (run as `binary`) lists
/// them, for a phone: only twr's own (this machine's system voices are not the phone's), and
/// ready once installed with what they need to be made (espeak-ng), whether or not this
/// machine has speakers.
pub async fn natural_listing(binary: &Path) -> Result<Value> {
    let out = tokio::process::Command::new(binary)
        .args(["voices", "list", "--json"])
        .env_remove("TWR_DEVICE_TOKEN")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("Listing twr's voices")?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{}",
            why.lines()
                .last()
                .unwrap_or("This hub's twr has no `twr voices`")
                .trim_start_matches("twr: ")
        );
    }
    let mut listing: Value = serde_json::from_slice(&out.stdout).context("Reading twr's voices")?;
    Ok(natural_only(&mut listing))
}

/// [`natural_listing`]'s filter over a listing.
pub fn natural_only(listing: &mut Value) -> Value {
    let mut models = Vec::new();
    for m in listing["models"].as_array().cloned().unwrap_or_default() {
        let mut m = m;
        let espeak = m["needs"].as_str().is_some_and(|n| n.contains("espeak-ng"));
        m["needs"] = if espeak {
            json!("espeak-ng")
        } else {
            Value::Null
        };
        models.push(m);
    }
    let ready = |model: &str| {
        models
            .iter()
            .any(|m| m["name"] == model && m["installed"] == true && m["needs"].is_null())
    };
    let voices: Vec<Value> = listing["voices"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v["engine"] == "twr")
        .map(|mut v| {
            v["ready"] = json!(ready(v["model"].as_str().unwrap_or_default()));
            v
        })
        .collect();
    json!({"voices": voices, "models": models, "espeak": listing["espeak"].clone()})
}

/// `twr voices install MODEL --json`, its progress told to the phone.
async fn install(out: &Outbox, binary: &Path, model: &str) -> Result<()> {
    use tokio::io::AsyncBufReadExt;
    let mut child = tokio::process::Command::new(binary)
        .args(["voices", "install", model, "--json"])
        .env_remove("TWR_DEVICE_TOKEN")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Starting the voice download")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut failure = None;
    let event = |install: Value| json!({"type":"voice-install","install":install});
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("progress") => out.send(event(json!({
                "model": model, "phase": "downloading",
                "bytes": v["bytes"], "total": v["total"],
            }))),
            Some("phase") => out.send(event(json!({"model": model, "phase": v["phase"]}))),
            Some("error") => failure = v["message"].as_str().map(str::to_owned),
            _ => {}
        }
    }
    let status = child.wait().await?;
    match (status.success(), failure) {
        (true, _) => Ok(()),
        (false, Some(why)) => Err(anyhow!(why)),
        (false, None) => Err(anyhow!("The voice download failed")),
    }
}

/// The phone's choice ([`SpeechState::voices`]) as a speaker's voices: the voice of other
/// languages is the English one, else the Spanish one.
pub fn chosen_voices(voices: &BTreeMap<String, String>) -> twr_speech::Voices {
    twr_speech::Voices {
        voice: voices.get("en").or_else(|| voices.get("es")).cloned(),
        voices: voices.clone(),
    }
}

/// Sets (or, with `None`, clears) the phone's voice for `lang` in its state file.
pub fn select(
    store: &StateStore,
    lang: &str,
    voice: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    anyhow::ensure!(language_ok(lang), "Unknown language");
    if let Some(v) = voice {
        anyhow::ensure!(
            twr_core::speech::name_error("voice", v, twr_speech::speaker::VOICE_MAX).is_none(),
            "Invalid voice"
        );
    }
    let state = store
        .update(|s| match voice {
            Some(v) => {
                s.voices.insert(lang.to_owned(), v.to_owned());
            }
            None => {
                s.voices.remove(lang);
            }
        })
        .context("Saving the phone's voice")?;
    Ok(state.voices)
}

/// The most audio of one `voice-audio` event, in bytes of PCM (about 2.7 s at 24 kHz): small
/// next to a socket's event backlog ([`super::events`]).
const PIECE_BYTES: usize = 128 * 1024;
/// How far the audio sent may run ahead of the time it takes to play.
const AHEAD: Duration = Duration::from_secs(8);

/// One piece of audio: base64 of 16-bit little-endian mono PCM at `rate`.
#[derive(Debug, Clone)]
pub struct Piece {
    pub pcm: Arc<str>,
    pub rate: u32,
}

impl Piece {
    /// How long it plays.
    fn secs(&self) -> f64 {
        let bytes = self.pcm.len() / 4 * 3;
        bytes as f64 / 2.0 / f64::from(self.rate.max(1))
    }
}

/// A chunk being made into audio, as far as it is.
#[derive(Debug, Clone, Default)]
pub struct Render {
    pub pieces: Vec<Piece>,
    pub done: bool,
    pub error: Option<String>,
}

/// `pcm` (a sentence's base64) cut into pieces of [`PIECE_BYTES`] at most (whole base64
/// quads, so each decodes alone).
fn pieces(pcm: &str, rate: u32) -> Vec<Piece> {
    let step = PIECE_BYTES / 3 * 4;
    let mut out = Vec::new();
    let mut rest = pcm;
    while !rest.is_empty() {
        let n = rest.len().min(step);
        let (head, tail) = rest.split_at(n);
        out.push(Piece {
            pcm: Arc::from(head),
            rate,
        });
        rest = tail;
    }
    out
}

/// What a request to [`Renderer`] is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenderKey {
    pub voice: String,
    pub text: String,
    pub lang: Option<String>,
    pub rate: u32,
}

struct RenderProc {
    stdin: mpsc::UnboundedSender<String>,
    routes: Arc<Mutex<HashMap<u64, watch::Sender<Render>>>>,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

/// twr's natural voices, made on this machine for phones: one `twr voices render` process
/// (its models stay loaded while it is used), started when first needed and again if it
/// ends. Its audio goes to the phone, never to this machine's speakers.
pub struct Renderer {
    binary: PathBuf,
    proc: Mutex<Option<RenderProc>>,
    next: AtomicU64,
}

impl Renderer {
    pub fn new(binary: PathBuf) -> Arc<Renderer> {
        Arc::new(Renderer {
            binary,
            proc: Mutex::new(None),
            next: AtomicU64::new(0),
        })
    }

    fn spawn(&self) -> Result<RenderProc> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut child = tokio::process::Command::new(&self.binary)
            .args(["voices", "render"])
            .env_remove("TWR_DEVICE_TOKEN")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Starting twr's voices")?;
        let mut stdin = child.stdin.take().context("twr's voices: no stdin")?;
        let stdout = child.stdout.take().context("twr's voices: no stdout")?;
        let stderr = child.stderr.take().context("twr's voices: no stderr")?;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let routes: Arc<Mutex<HashMap<u64, watch::Sender<Render>>>> = Arc::default();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                crate::gateway_log!(
                    "twr voices render: {}",
                    line.chars().take(300).collect::<String>()
                );
            }
        });
        {
            let (routes, alive) = (routes.clone(), alive.clone());
            tokio::spawn(async move {
                let mut child = child;
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(id) = v["id"].as_u64() else { continue };
                    let mut routes = routes.lock().unwrap();
                    let Some(route) = routes.get(&id) else {
                        continue;
                    };
                    let finished = if let Some(pcm) = v["pcm"].as_str() {
                        let rate = v["rate"]
                            .as_u64()
                            .and_then(|r| u32::try_from(r).ok())
                            .unwrap_or(24_000);
                        route.send_modify(|r| r.pieces.extend(pieces(pcm, rate)));
                        false
                    } else if v["done"] == true {
                        route.send_modify(|r| r.done = true);
                        true
                    } else {
                        let why = v["error"].as_str().unwrap_or("the voice failed").to_owned();
                        route.send_modify(|r| r.error = Some(why));
                        true
                    };
                    if finished {
                        routes.remove(&id);
                    }
                }
                alive.store(false, Ordering::SeqCst);
                let _ = child.kill().await;
                for (_, route) in routes.lock().unwrap().drain() {
                    route.send_modify(|r| r.error = Some("twr's voices stopped".into()));
                }
            });
        }
        Ok(RenderProc {
            stdin: tx,
            routes,
            alive,
        })
    }

    /// Starts making `key` into audio: what there is so far, as it grows.
    pub fn render(&self, key: &RenderKey) -> (u64, watch::Receiver<Render>) {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = watch::channel(Render::default());
        let mut proc = self.proc.lock().unwrap();
        if proc
            .as_ref()
            .is_none_or(|p| !p.alive.load(Ordering::SeqCst))
        {
            match self.spawn() {
                Ok(p) => *proc = Some(p),
                Err(e) => {
                    tx.send_modify(|r| r.error = Some(format!("{e:#}")));
                    return (id, rx);
                }
            }
        }
        let p = proc.as_ref().expect("started");
        p.routes.lock().unwrap().insert(id, tx);
        // It ended meanwhile (a twr without its own voices ends at once): its reader may have
        // told the routes so before this one was there.
        if !p.alive.load(Ordering::SeqCst) {
            if let Some(route) = p.routes.lock().unwrap().remove(&id) {
                route.send_modify(|r| r.error = Some("twr's voices stopped".into()));
            }
            return (id, rx);
        }
        let line = json!({"id": id, "voice": key.voice, "text": key.text, "lang": key.lang, "rate": key.rate});
        let _ = p.stdin.send(format!("{line}\n"));
        (id, rx)
    }

    /// Stops making request `id` (its audio is no longer wanted).
    pub fn cancel(&self, id: u64) {
        if let Some(p) = self.proc.lock().unwrap().as_ref() {
            if p.routes.lock().unwrap().remove(&id).is_some() {
                let _ = p
                    .stdin
                    .send(format!("{}\n", json!({"id": id, "cancel": true})));
            }
        }
    }
}

/// A chunk made (or being made) for the phone, kept so a pause's repeat or the chunk
/// [`Engine::prepare`] announced isn't made twice.
struct Made {
    key: RenderKey,
    id: u64,
    render: watch::Receiver<Render>,
}

/// The phone's voice, as a speaker's engine (see the module documentation).
pub struct DeviceEngine {
    out: Arc<Outbox>,
    renderer: Option<Arc<Renderer>>,
    /// The chunks made last (the one said, and the next one): at most two.
    made: Vec<Made>,
}

impl DeviceEngine {
    pub fn new(out: Arc<Outbox>, renderer: Option<Arc<Renderer>>) -> DeviceEngine {
        DeviceEngine {
            out,
            renderer,
            made: Vec::new(),
        }
    }

    /// `key`'s audio: as made before, or started now. Older chunks are forgotten (and
    /// stopped if still being made).
    fn made(&mut self, key: RenderKey) -> Option<watch::Receiver<Render>> {
        let renderer = self.renderer.clone()?;
        if let Some(m) = self.made.iter().find(|m| m.key == key) {
            return Some(m.render.clone());
        }
        let (id, render) = renderer.render(&key);
        self.made.push(Made {
            key,
            id,
            render: render.clone(),
        });
        while self.made.len() > 2 {
            let old = self.made.remove(0);
            if !old.render.borrow().done {
                renderer.cancel(old.id);
            }
        }
        Some(render)
    }
}

fn render_key(chunk: &str, prefs: &Prefs) -> Option<RenderKey> {
    let voice = prefs.voice.as_deref().filter(|v| is_natural(v))?;
    Some(RenderKey {
        voice: voice.to_owned(),
        text: chunk.to_owned(),
        lang: prefs.lang.clone(),
        rate: prefs.rate,
    })
}

/// Sends `render`'s audio to the phone as utterance `id`, as it is made, no faster than
/// [`AHEAD`] before it plays; the last event says it is the last.
async fn send_audio(out: &Outbox, id: u64, mut render: watch::Receiver<Render>) -> Result<()> {
    let start = tokio::time::Instant::now();
    let mut sent = 0usize;
    let mut played = 0f64;
    loop {
        let state = render
            .wait_for(|r| r.pieces.len() > sent || r.done || r.error.is_some())
            .await
            .map_err(|_| anyhow!("twr's voices stopped"))?
            .clone();
        if state.pieces.len() > sent {
            let piece = &state.pieces[sent];
            let due = start + Duration::from_secs_f64(played);
            tokio::time::sleep_until(due.checked_sub(AHEAD).unwrap_or(due)).await;
            out.events
                .send(json!({"type":"voice-audio","id":id,"seq":sent,"rate":piece.rate,"pcm":&*piece.pcm,"last":false}))
                .await
                .map_err(|_| anyhow!("the phone's connection closed"))?;
            played += piece.secs();
            sent += 1;
            continue;
        }
        if let Some(why) = state.error {
            bail!("natural voice: {why}");
        }
        out.events
            .send(
                json!({"type":"voice-audio","id":id,"seq":sent,"rate":24_000,"pcm":"","last":true}),
            )
            .await
            .map_err(|_| anyhow!("the phone's connection closed"))?;
        return Ok(());
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
        let (voice, rate, lang) = (prefs.voice.clone(), prefs.rate, prefs.lang.clone());
        let audio = render_key(chunk, prefs).and_then(|key| self.made(key));
        if self.renderer.is_none() && voice.as_deref().is_some_and(is_natural) {
            return Box::pin(async { bail!("natural voices are not available for this phone") });
        }
        Box::pin(async move {
            let mut sockets = out.sockets.subscribe();
            let deadline = tokio::time::Instant::now() + utterance_budget(&text);
            loop {
                let open = tokio::time::timeout(SOCKET_WAIT, sockets.wait_for(|n| *n > 0))
                    .await
                    .map(|r| r.is_ok());
                if open != Ok(true) {
                    bail!("the phone is not connected");
                }
                // Each attempt is an utterance of its own: one said again after a lost socket
                // starts over on the phone (its audio from the first piece).
                let id = out.next.fetch_add(1, Ordering::SeqCst) + 1;
                let (tx, mut rx) = oneshot::channel();
                out.pending.lock().unwrap().insert(id, tx);
                let mut utterance = Utterance {
                    out: out.clone(),
                    id,
                    sent: true,
                    done: false,
                };
                // A natural voice's audio goes as it is made, while the ack is waited for.
                let sending = async {
                    match &audio {
                        Some(render) => send_audio(&out, id, render.clone()).await?,
                        None => out
                            .events
                            .send(json!({"type":"voice-speak","id":id,"text":text,"voice":voice,"rate":rate,"lang":lang}))
                            .await
                            .map_err(|_| anyhow!("the phone's connection closed"))?,
                    }
                    std::future::pending::<Result<()>>().await
                };
                tokio::pin!(sending);
                tokio::select! {
                    failed = &mut sending => return failed,
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

    fn prepare(&mut self, next: &str, prefs: &Prefs) {
        if let Some(key) = render_key(next, prefs) {
            let _ = self.made(key);
        }
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
    /// Makes twr's natural voices for it (`None`: only the phone's own voices).
    pub renderer: Option<Arc<Renderer>>,
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
    /// A natural voice's preview being sent ([`DeviceVoice::preview`]).
    preview: Mutex<Option<tokio::task::AbortHandle>>,
    /// The model downloads under way, by model.
    installs: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl DeviceVoice {
    pub fn new(events: mpsc::Sender<Value>) -> DeviceVoice {
        DeviceVoice {
            out: Outbox::new(events),
            running: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new(Status::default())),
            starts: AtomicU64::new(0),
            changing: tokio::sync::Mutex::new(()),
            preview: Mutex::new(None),
            installs: Arc::default(),
        }
    }

    /// Sends the phone a sample sentence in `lang` said by twr's natural `voice`, made here
    /// (the phone says its own voices' samples itself). The utterance id it plays as: its ack
    /// is ignored. A preview still being sent is dropped.
    pub fn preview(&self, renderer: &Arc<Renderer>, voice: &str, lang: &str) -> Result<u64> {
        anyhow::ensure!(is_natural(voice), "Not one of twr's natural voices");
        anyhow::ensure!(language_ok(lang), "Unknown language");
        self.stop_preview();
        let key = RenderKey {
            voice: voice.to_owned(),
            text: preview_text(lang).to_owned(),
            lang: Some(lang.to_owned()),
            rate: 0,
        };
        let (render_id, render) = renderer.render(&key);
        let id = self.out.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (out, renderer) = (self.out.clone(), renderer.clone());
        let task = tokio::spawn(async move {
            struct Cancel(Arc<Renderer>, u64);
            impl Drop for Cancel {
                fn drop(&mut self) {
                    self.0.cancel(self.1);
                }
            }
            let _cancel = Cancel(renderer, render_id);
            if let Err(e) = send_audio(&out, id, render).await {
                out.send(json!({"type":"voice-preview-error","id":id,"error":format!("{e:#}")}));
            }
        });
        *self.preview.lock().unwrap() = Some(task.abort_handle());
        Ok(id)
    }

    /// Stops sending a preview.
    pub fn stop_preview(&self) {
        if let Some(task) = self.preview.lock().unwrap().take() {
            task.abort();
        }
    }

    /// Starts downloading twr's voice `model` on this machine (`twr voices install --json`,
    /// run as `binary`) and returns: its progress, then `done` or `error`, reach the phone as
    /// `voice-install` events (a phone's calls are answered one at a time, and a download
    /// takes minutes). One under way for `model` is refused.
    pub fn start_install(&self, binary: PathBuf, model: &str) -> Result<()> {
        anyhow::ensure!(
            !model.is_empty()
                && model.len() <= 32
                && model
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "Unknown voice model"
        );
        if !self.installs.lock().unwrap().insert(model.to_owned()) {
            bail!("That voice is already downloading");
        }
        let (out, installs, model) = (self.out.clone(), self.installs.clone(), model.to_owned());
        tokio::spawn(async move {
            let result = install(&out, &binary, &model).await;
            installs.lock().unwrap().remove(&model);
            // Waited for (not dropped like progress): the phone waits for it.
            let _ = out
                .events
                .send(json!({"type":"voice-install","install": match &result {
                    Ok(()) => json!({"model": model, "phase": "done"}),
                    Err(e) => json!({"model": model, "phase": "error", "error": format!("{e:#}")}),
                }}))
                .await;
        });
        Ok(())
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
        // The voices are the phone's own choice, followed while it speaks; the hub machine's
        // `[speech] voice` and `voices` name its voices, not the phone's.
        let mut config = start.config;
        config.voice.clear();
        config.voices.clear();
        let voices_source: twr_speech::VoicesSource = {
            let store = start.store.clone();
            let last = Mutex::new(None::<BTreeMap<String, String>>);
            Arc::new(move || {
                let voices = store.load().ok()?.voices;
                let mut last = last.lock().unwrap();
                if last.as_ref() == Some(&voices) {
                    return None;
                }
                *last = Some(voices.clone());
                Some(chosen_voices(&voices))
            })
        };
        let opts = SpeakerOpts {
            name: start.name.clone(),
            config,
            voice: None,
            rate: None,
            voices: Default::default(),
            voices_source: Some(voices_source),
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
        let engine = Box::new(DeviceEngine::new(self.out.clone(), start.renderer.clone()));
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
        let mut engine = DeviceEngine::new(out.clone(), None);
        let prefs = Prefs {
            voice: Some("Ava".into()),
            rate: 180,
            volume: 1.0,
            ..Prefs::default()
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
        let mut engine = DeviceEngine::new(out.clone(), None);
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
        let mut engine = DeviceEngine::new(out.clone(), None);
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
        // An utterance of its own (a natural voice's audio starts over on the phone): a late
        // ack of the first one is ignored.
        let second = again["id"].as_u64().unwrap();
        assert_ne!(second, first);
        assert_eq!(again["text"], "Said twice.");
        assert!(!out.ack(first, Ok(())));
        assert!(out.ack(second, Ok(())));
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
            self.start_with(allowed, SpeechConfig::default()).await
        }
        async fn start_with(
            &mut self,
            allowed: Arc<dyn Fn() -> bool + Send + Sync>,
            config: SpeechConfig,
        ) -> Value {
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
                        config,
                        renderer: None,
                        allowed,
                    })
                })
                .await
                .unwrap()
        }
        /// The hub attaches the speaker, live, and sends `text` as message `seq`.
        async fn say(&mut self, seq: u64, text: &str) {
            let hub = self.hub.as_mut().unwrap();
            if seq == 2 {
                hub.events
                    .send(ManualEvent::Connect {
                        ws: "work".into(),
                        ack: SpeechAttachAck {
                            orchestrator: Some("T".into()),
                            head: 1,
                        },
                    })
                    .unwrap();
                hub.attaches.recv().await.unwrap();
            }
            hub.events
                .send(ManualEvent::Item {
                    ws: "work".into(),
                    item: msg(seq, text),
                })
                .unwrap();
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

    /// A stand-in for `twr voices render`: one piece of audio per request, then done.
    fn fake_renderer(dir: &Path) -> Arc<Renderer> {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("twr");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             while IFS= read -r line; do\n\
               case \"$line\" in *'\"cancel\":true'*) continue;; esac\n\
               id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n\
               printf '{\"id\":%s,\"seq\":0,\"rate\":24000,\"pcm\":\"AAAA\"}\\n' \"$id\"\n\
               printf '{\"id\":%s,\"done\":true}\\n' \"$id\"\n\
             done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Renderer::new(script)
    }

    #[tokio::test]
    async fn a_natural_voice_is_made_here_and_sent_as_audio() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let out = Outbox::new(tx);
        let _socket = out.socket();
        let mut engine = DeviceEngine::new(out.clone(), Some(fake_renderer(dir.path())));
        let prefs = Prefs {
            voice: Some("kokoro/af_heart".into()),
            lang: Some("en".into()),
            ..Prefs::default()
        };
        let said = tokio::spawn(engine.speak("Hello there.", &prefs));
        let first = next_event(&mut rx, "voice-audio").await;
        assert_eq!(
            (first["seq"].as_u64(), first["last"].as_bool()),
            (Some(0), Some(false))
        );
        assert_eq!(
            (first["pcm"].as_str(), first["rate"].as_u64()),
            (Some("AAAA"), Some(24_000))
        );
        let last = next_event(&mut rx, "voice-audio").await;
        assert_eq!(last["id"], first["id"]);
        assert_eq!(
            (last["seq"].as_u64(), last["last"].as_bool()),
            (Some(1), Some(true))
        );
        assert!(out.ack(first["id"].as_u64().unwrap(), Ok(())));
        said.await.unwrap().unwrap();
        // The text never goes to the phone for a natural voice.
        while let Ok(e) = rx.try_recv() {
            assert_ne!(e["type"], "voice-speak");
        }
        // Without a renderer (an older gateway's speaker), a natural voice fails at once.
        let mut bare = DeviceEngine::new(out.clone(), None);
        let e = bare.speak("Hi.", &prefs).await.unwrap_err();
        assert!(format!("{e:#}").contains("not available"));
    }

    #[tokio::test]
    async fn a_phone_voice_gets_the_message_language() {
        let (tx, mut rx) = mpsc::channel(16);
        let out = Outbox::new(tx);
        let _socket = out.socket();
        let mut engine = DeviceEngine::new(out.clone(), None);
        let prefs = Prefs {
            voice: Some("com.apple.voice.premium.es-MX.Paulina".into()),
            lang: Some("es".into()),
            ..Prefs::default()
        };
        let said = tokio::spawn(engine.speak("Hola.", &prefs));
        let e = next_event(&mut rx, "voice-speak").await;
        assert_eq!(e["lang"], "es");
        assert_eq!(e["voice"], "com.apple.voice.premium.es-MX.Paulina");
        out.ack(e["id"].as_u64().unwrap(), Ok(()));
        said.await.unwrap().unwrap();
    }

    /// The phone reads with its own choice, never the hub machine's `[speech]` voices, and a
    /// choice made while its speaker runs is heard from the next message.
    #[tokio::test]
    async fn the_phone_speaker_reads_with_the_phone_s_own_voices() {
        let mut rig = rig();
        let _socket = rig.voice.socket();
        select(&rig.store, "en", Some("com.apple.voice.premium.en-US.Ava")).unwrap();
        let config = SpeechConfig {
            voice: "Ava (Premium)".into(),
            voices: [("en".to_string(), "Samantha".to_string())].into(),
            ..SpeechConfig::default()
        };
        rig.start_with(Arc::new(|| true), config).await;
        rig.say(2, "The tests pass.").await;
        let e = next_event(&mut rig.events, "voice-speak").await;
        assert_eq!(e["voice"], "com.apple.voice.premium.en-US.Ava");
        rig.voice
            .control(Op::Said(e["id"].as_u64().unwrap()), || unreachable!())
            .await
            .unwrap();
        select(
            &rig.store,
            "en",
            Some("com.apple.voice.enhanced.en-GB.Daniel"),
        )
        .unwrap();
        select(&rig.store, "en", None).unwrap();
        select(&rig.store, "es", Some("es-us-x-esd-local")).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        rig.say(3, "The build is green.").await;
        let e = next_event(&mut rig.events, "voice-speak").await;
        // English cleared: the Spanish choice is the voice of other languages too.
        assert_eq!(e["voice"], "es-us-x-esd-local");
        rig.voice
            .control(Op::Stop, || unreachable!())
            .await
            .unwrap();
    }

    #[test]
    fn choices_listings_and_pieces() {
        assert!(is_natural("kokoro/af_heart") && is_natural("piper-es/claude"));
        for not in [
            "Ava (Premium)",
            "com.apple.voice.premium.en-US.Ava",
            "es-us-x-esd-local",
            "/x",
            "x/",
            "Big/One",
        ] {
            assert!(!is_natural(not), "{not}");
        }
        let dir = tempfile::tempdir().unwrap();
        let store = state_store(dir.path(), "phone-1").unwrap();
        assert!(select(&store, "fr", Some("Thomas")).is_err());
        assert!(select(&store, "en", Some("a\u{200b}b")).is_err());
        assert!(select(&store, "en", Some(&"x".repeat(129))).is_err());
        let chosen = select(&store, "es", Some("kokoro/ef_dora")).unwrap();
        assert_eq!(
            chosen_voices(&chosen).voice.as_deref(),
            Some("kokoro/ef_dora")
        );
        assert_eq!(
            preview_text("es"),
            "Hola. Así sueno cuando leo tus respuestas en voz alta."
        );
        // Only twr's voices, ready without this machine's speakers, espeak-ng still needed.
        let mut listing = json!({
            "voices": [
                {"id": "Ava (Premium)", "engine": "system", "ready": true},
                {"id": "kokoro/af_heart", "engine": "twr", "model": "kokoro", "ready": false},
                {"id": "piper-en/lessac", "engine": "twr", "model": "piper-en", "ready": false},
            ],
            "models": [
                {"name": "kokoro", "installed": true, "needs": "an audio output device"},
                {"name": "piper-en", "installed": true, "needs": "espeak-ng and an audio output device"},
            ],
            "espeak": null,
        });
        let phone = natural_only(&mut listing);
        let ids: Vec<_> = phone["voices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v["id"].as_str().unwrap(), v["ready"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            ids,
            vec![("kokoro/af_heart", true), ("piper-en/lessac", false)]
        );
        assert_eq!(phone["models"][0]["needs"], Value::Null);
        assert_eq!(phone["models"][1]["needs"], "espeak-ng");
        // Audio is cut in whole base64 quads, each small next to a socket's backlog.
        let pcm = "A".repeat(PIECE_BYTES / 3 * 4 * 2 + 8);
        let cut = pieces(&pcm, 24_000);
        assert_eq!(cut.len(), 3);
        assert!(cut
            .iter()
            .all(|p| p.pcm.len() % 4 == 0 && p.pcm.len() <= PIECE_BYTES / 3 * 4));
        assert_eq!(cut.iter().map(|p| p.pcm.len()).sum::<usize>(), pcm.len());
        assert!((cut[0].secs() - (PIECE_BYTES / 3 * 3) as f64 / 48_000.0).abs() < 1e-9);
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
