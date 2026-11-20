//! `twr voices`: the voices that read messages aloud (docs/read-aloud.md#voices).
//!
//! - `list`: this machine's own voices (macOS's, `say -v ?`, its Premium and Enhanced ones
//!   marked) and twr's local neural ones ([`twr_neural::MODELS`]), installed or not, with how
//!   fast each installed model runs here. `--json` for a program (the desktop app).
//! - `install MODEL`: downloads the catalog's archive over HTTPS, checking its size and
//!   SHA-256 as it comes, into a private file next to the models; then unpacks, checks and
//!   times it (`twr_neural::install`). `--json` reports progress as JSON lines on stdout.
//! - `remove MODEL`.
//! - `try VOICE [TEXT]`: says a sentence with a voice, as a speaker would; `--out FILE` writes
//!   a WAV file instead (twr's voices).
//! - `render`: makes speech with twr's voices for another program, which plays it elsewhere
//!   (the mobile gateway, for a phone that reads aloud with one of them): requests as JSON
//!   lines on stdin, each sentence's audio as a JSON line on stdout ([`render`]). Nothing
//!   plays here.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::bail;
#[cfg(feature = "neural")]
use anyhow::{anyhow, Context};
use clap::Subcommand;
use serde::Serialize;
use serde_json::json;
use twr_core::config::{Config, SpeechEngine};
use twr_core::paths::Paths;

/// `twr voices`' subcommands.
#[derive(Debug, Subcommand)]
pub enum VoicesCmd {
    /// The voices for reading aloud: this machine's and twr's own (installed or not).
    List {
        /// Every language of this machine's voices, not only the ones twr tells apart.
        #[arg(long)]
        all: bool,
        /// One JSON object on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Download and install one of twr's voice models (`twr voices list` names them).
    Install {
        /// The model: kokoro, supertonic, piper-en or piper-es.
        model: String,
        /// Progress as JSON lines on stdout (`{"type":"progress",...}`), then the result.
        #[arg(long)]
        json: bool,
    },
    /// Remove one of twr's voice models.
    Remove {
        /// The model.
        model: String,
    },
    /// Make speech with twr's voices for another program (the mobile gateway): requests as
    /// JSON lines on stdin, audio as JSON lines on stdout. Nothing plays here.
    #[command(hide = true)]
    Render,
    /// Say a sentence with a voice, as a speaker would (`kokoro/af_heart`, `Ava (Premium)`).
    Try {
        /// The voice.
        voice: String,
        /// What to say; a sentence in the voice's language by default.
        text: Option<String>,
        /// Write the audio to this WAV file instead of playing it (twr's voices only).
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
}

/// Why this twr has no voices of its own.
#[cfg(not(feature = "neural"))]
const NO_NEURAL: &str = "this twr was built without its own voices (the `neural` feature; the \
                         static Linux release has none): build twr from source, or use this \
                         machine's voices";

/// The catalog's model named `name`, or what the names are.
#[cfg(feature = "neural")]
fn model(name: &str) -> anyhow::Result<&'static twr_neural::ModelSpec> {
    twr_neural::model(name).ok_or_else(|| {
        let names: Vec<_> = twr_neural::MODELS.iter().map(|m| m.name).collect();
        anyhow!("no model {name:?}: {}", names.join(", "))
    })
}

/// One voice of the listing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VoiceRow {
    /// What `[speech] voice`, `voices` and `--voice` take.
    pub id: String,
    /// What to show.
    pub name: String,
    /// `system` (the machine's own engine) or `twr` (a local neural model).
    pub engine: &'static str,
    /// Its model (twr's voices).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'static str>,
    /// The languages it reads well.
    pub languages: Vec<String>,
    /// Its accent (`en-US`; `mul` for a voice of several languages).
    pub locale: String,
    /// `premium`, `enhanced` or `default` (macOS's), `neural` (twr's).
    pub quality: &'static str,
    /// Whether it can speak now: a system voice is; one of twr's when its model is installed
    /// and what it needs is there.
    pub ready: bool,
}

/// One of twr's models in the listing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelRow {
    pub name: &'static str,
    pub title: &'static str,
    pub installed: bool,
    /// The download, in bytes and for people.
    pub bytes: u64,
    pub size: String,
    pub license: &'static str,
    /// Seconds it takes to make a second of speech here (timed when installed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtf: Option<f32>,
    /// Whether it keeps up here with room to spare.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<bool>,
    /// What it needs that this machine lacks (`espeak-ng`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs: Option<String>,
}

/// `twr voices list --json`.
#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub voices: Vec<VoiceRow>,
    pub models: Vec<ModelRow>,
    /// The system's engine `auto` uses (`say`, `spd`), if any.
    pub system: Option<String>,
    /// The espeak-ng program twr's Kokoro and Piper voices spell with, if found.
    pub espeak: Option<String>,
    /// Whether there is an output device for twr's voices.
    pub audio: bool,
    /// `[speech] voice` and `voices`.
    pub voice: String,
    pub by_language: std::collections::BTreeMap<String, String>,
}

/// macOS's novelty voices (`say -v ?` lists them with the rest): never offered.
const NOVELTY: &[&str] = &[
    "Albert",
    "Bad News",
    "Bahh",
    "Bells",
    "Boing",
    "Bubbles",
    "Cellos",
    "Good News",
    "Jester",
    "Organ",
    "Superstar",
    "Trinoids",
    "Whisper",
    "Wobble",
    "Zarvox",
    "Deranged",
    "Hysterical",
    "Pipe Organ",
    "Fred",
    "Junior",
    "Kathy",
    "Ralph",
];

/// The voices `say -v ?` lists (`Ava (Premium)   en_US    # Hello! My name is Ava.`), in
/// `langs` (all: `None`), without the novelty ones.
pub fn parse_say_voices(listing: &str, langs: Option<&[&str]>) -> Vec<VoiceRow> {
    let mut out = Vec::new();
    for line in listing.lines() {
        let Some((head, _sample)) = line.split_once('#') else {
            continue;
        };
        let head = head.trim_end();
        let Some(split) = head.rfind(char::is_whitespace) else {
            continue;
        };
        let (name, locale) = (head[..split].trim(), head[split..].trim());
        let valid_locale = locale.len() >= 4
            && locale.contains('_')
            && locale
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if name.is_empty() || !valid_locale {
            continue;
        }
        let base = name.split(" (").next().unwrap_or(name);
        if NOVELTY.contains(&base) {
            continue;
        }
        let lang = locale.split('_').next().unwrap_or("").to_string();
        if langs.is_some_and(|l| !l.contains(&lang.as_str())) {
            continue;
        }
        let quality = if name.contains("(Premium)") {
            "premium"
        } else if name.contains("(Enhanced)") {
            "enhanced"
        } else {
            "default"
        };
        let row = VoiceRow {
            id: name.to_string(),
            name: name.to_string(),
            engine: "system",
            model: None,
            languages: vec![lang],
            locale: locale.replace('_', "-"),
            quality,
            ready: true,
        };
        // `say -v ?` lists some voices twice.
        if !out.iter().any(|r: &VoiceRow| r.id == row.id) {
            out.push(row);
        }
    }
    // The best first, then by name.
    let rank = |q: &str| match q {
        "premium" => 0,
        "enhanced" => 1,
        _ => 2,
    };
    out.sort_by(|a, b| {
        (a.languages[0].as_str(), rank(a.quality), a.name.as_str()).cmp(&(
            b.languages[0].as_str(),
            rank(b.quality),
            b.name.as_str(),
        ))
    });
    out
}

/// The listing for this machine.
pub fn listing(
    config: &Config,
    data_dir: &Path,
    path: Option<&std::ffi::OsStr>,
    all: bool,
) -> Listing {
    let speech = &config.speech;
    let system =
        match twr_speech::engine::resolve(SpeechEngine::Auto, &[], std::env::consts::OS, path) {
            Ok(twr_speech::engine::Kind::Say(_)) => Some("say".to_string()),
            Ok(twr_speech::engine::Kind::Spd(_)) => Some("spd".to_string()),
            _ => None,
        };
    let langs = (!all).then_some(twr_core::speech::LANGUAGES);
    let mut voices = Vec::new();
    if system.as_deref() == Some("say") {
        if let Ok(out) = std::process::Command::new("say").args(["-v", "?"]).output() {
            voices = parse_say_voices(&String::from_utf8_lossy(&out.stdout), langs);
        }
    }
    #[cfg(feature = "neural")]
    let (models, espeak, audio) = neural_listing(speech, data_dir, path, &mut voices);
    #[cfg(not(feature = "neural"))]
    let (models, espeak, audio) = {
        let _ = data_dir;
        (Vec::new(), None::<PathBuf>, false)
    };
    Listing {
        voices,
        models,
        system,
        espeak: espeak.map(|p| p.display().to_string()),
        audio,
        voice: speech.voice.clone(),
        by_language: speech.voices.clone(),
    }
}

/// twr's models and voices, and what they need here (espeak-ng, an output device).
#[cfg(feature = "neural")]
fn neural_listing(
    speech: &twr_core::config::SpeechConfig,
    data_dir: &Path,
    path: Option<&std::ffi::OsStr>,
    voices: &mut Vec<VoiceRow>,
) -> (Vec<ModelRow>, Option<PathBuf>, bool) {
    let espeak = twr_neural::phonemes::find_espeak(&speech.espeak, path);
    let audio = twr_neural::audio::has_output();
    let mut models = Vec::new();
    for m in twr_neural::MODELS {
        let installed = twr_neural::install::installed(data_dir, m);
        let needs_espeak = m.family != twr_neural::Family::Supertonic && espeak.is_none();
        let needs = match (needs_espeak, !audio) {
            (true, true) => Some("espeak-ng and an audio output device".to_string()),
            (true, false) => Some("espeak-ng".to_string()),
            (false, true) => Some("an audio output device".to_string()),
            (false, false) => None,
        };
        let ready = installed.is_some() && needs.is_none();
        for v in m.voices {
            voices.push(VoiceRow {
                id: twr_neural::voice_name(m, v),
                name: format!("{} {}", m.title, v.title),
                engine: "twr",
                model: Some(m.name),
                languages: v.languages.iter().map(|l| l.to_string()).collect(),
                locale: v.locale.to_string(),
                quality: "neural",
                ready,
            });
        }
        models.push(ModelRow {
            name: m.name,
            title: m.title,
            installed: installed.is_some(),
            bytes: m.bytes,
            size: m.size(),
            license: m.license,
            rtf: installed.as_ref().and_then(|i| i.rtf),
            fast: installed.as_ref().and_then(|i| i.fast()),
            needs,
        });
    }
    (models, espeak, audio)
}

/// Runs `twr voices`.
pub async fn run(cmd: VoicesCmd, paths: &Paths) -> anyhow::Result<ExitCode> {
    let root = paths.root();
    let config = Config::load_from(&root)?;
    let path = std::env::var_os("PATH");
    let data_dir = root.data_dir.clone();
    match cmd {
        VoicesCmd::List { all, json } => {
            let l = listing(&config, &data_dir, path.as_deref(), all);
            if json {
                println!("{}", serde_json::to_string(&l)?);
            } else {
                print_listing(&l);
            }
        }
        #[cfg(feature = "neural")]
        VoicesCmd::Install { model: name, json } => {
            let result = match model(&name) {
                Ok(spec) => {
                    install(
                        spec,
                        &data_dir,
                        &config.speech.espeak,
                        path.as_deref(),
                        json,
                    )
                    .await
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => {}
                Err(e) if json => {
                    println!("{}", json!({"type": "error", "message": format!("{e:#}")}));
                    return Ok(ExitCode::FAILURE);
                }
                Err(e) => return Err(e),
            }
        }
        #[cfg(feature = "neural")]
        VoicesCmd::Remove { model: name } => {
            let spec = model(&name)?;
            if twr_neural::install::remove(&data_dir, spec)? {
                println!("voices: removed {}", spec.title);
            } else {
                println!("voices: {} was not installed", spec.title);
            }
        }
        #[cfg(not(feature = "neural"))]
        VoicesCmd::Install { json: true, .. } => {
            println!("{}", json!({"type": "error", "message": NO_NEURAL}));
            return Ok(ExitCode::FAILURE);
        }
        #[cfg(not(feature = "neural"))]
        VoicesCmd::Install { .. } | VoicesCmd::Remove { .. } | VoicesCmd::Render => {
            bail!(NO_NEURAL)
        }
        #[cfg(feature = "neural")]
        VoicesCmd::Render => {
            let espeak = twr_neural::phonemes::find_espeak(&config.speech.espeak, path.as_deref());
            render(
                twr_neural::synth::Synth::start(data_dir.clone(), espeak),
                tokio::io::stdin(),
                tokio::io::stdout(),
            )
            .await?;
        }
        VoicesCmd::Try { voice, text, out } => {
            let text = text.unwrap_or_else(|| sample_text(&voice).to_string());
            match out {
                #[cfg(feature = "neural")]
                Some(out) => write_sample(
                    &data_dir,
                    &config.speech.espeak,
                    path.as_deref(),
                    &voice,
                    &text,
                    &out,
                )?,
                #[cfg(not(feature = "neural"))]
                Some(_) => bail!(NO_NEURAL),
                None => {
                    let mut engine = twr_speech::from_config(
                        &config.speech,
                        Some(SpeechEngine::Auto),
                        &data_dir,
                    )?;
                    twr_speech::engine::try_voice(engine.as_mut(), &voice, &text).await?;
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_listing(l: &Listing) {
    println!(
        "this machine's voices ({}):",
        l.system.as_deref().unwrap_or("none")
    );
    let system: Vec<_> = l.voices.iter().filter(|v| v.engine == "system").collect();
    if system.is_empty() {
        println!("  (none listed)");
    }
    for v in system {
        let q = if v.quality == "default" {
            String::new()
        } else {
            format!(", {}", v.quality)
        };
        println!("  {}  ({}{q})", v.id, v.locale);
    }
    println!("\ntwr's voices (`twr voices install MODEL`):");
    for m in &l.models {
        let state = match (m.installed, m.fast, &m.needs) {
            (false, _, _) => format!("not installed, {}", m.size),
            (true, _, Some(n)) => format!("installed, needs {n}"),
            (true, Some(true), None) => "installed, fast enough here".to_string(),
            (true, Some(false), None) => "installed, slow on this machine".to_string(),
            (true, None, None) => "installed".to_string(),
        };
        println!("  {} ({}): {state}", m.name, m.title);
        let voices: Vec<_> = l
            .voices
            .iter()
            .filter(|v| v.model == Some(m.name))
            .map(|v| format!("{} [{}]", v.id, v.languages.join(",")))
            .collect();
        println!("    {}", voices.join("  "));
    }
    #[cfg(feature = "neural")]
    if l.espeak.is_none() {
        println!(
            "\nKokoro and Piper need espeak-ng: {}",
            twr_neural::phonemes::espeak_hint()
        );
    }
    #[cfg(not(feature = "neural"))]
    println!("  (none: {NO_NEURAL})");
}

/// What `try` says by default: a sentence in the voice's language.
fn sample_text(voice: &str) -> &'static str {
    #[cfg(feature = "neural")]
    let neural = twr_neural::voice(voice).map(|(_, v)| v.languages[0] == "es");
    #[cfg(not(feature = "neural"))]
    let neural: Option<bool> = None;
    let spanish = match neural {
        Some(es) => es,
        None => {
            let v = voice.to_lowercase();
            v.contains("spanish")
                || ["mónica", "paulina", "marisol", "jorge", "juan", "diego"]
                    .iter()
                    .any(|n| v.starts_with(n))
        }
    };
    if spanish {
        "Hola. Así sueno cuando leo tus respuestas en voz alta."
    } else {
        "Hi. This is how I sound when I read your replies aloud."
    }
}

#[cfg(feature = "neural")]
/// `try --out`: `text` with twr's `voice`, as one WAV file.
fn write_sample(
    data_dir: &Path,
    espeak: &str,
    path: Option<&std::ffi::OsStr>,
    voice: &str,
    text: &str,
    out: &Path,
) -> anyhow::Result<()> {
    let (model, v) = twr_neural::voice(voice)
        .ok_or_else(|| anyhow!("--out takes one of twr's voices (`twr voices list`)"))?;
    let espeak = twr_neural::phonemes::find_espeak(espeak, path);
    let (clips, took) =
        twr_neural::synth::say_now(&model.path(data_dir), espeak.as_deref(), model, v, text)?;
    let rate = clips.first().map(|c| c.sample_rate).unwrap_or(24_000);
    let all = twr_neural::synth::Clip {
        samples: clips.into_iter().flat_map(|c| c.samples).collect(),
        sample_rate: rate,
    };
    std::fs::write(out, all.wav(1.0)).with_context(|| format!("writing {}", out.display()))?;
    eprintln!(
        "voices: {:.1} s of speech in {:.2} s, in {}",
        all.secs(),
        took.as_secs_f32(),
        out.display()
    );
    Ok(())
}

/// The sample rate of [`render`]'s audio, whatever the model's: 24 kHz mono 16-bit is 48 KB
/// a second, and enough for speech.
#[cfg(feature = "neural")]
pub const RENDER_RATE: u32 = 24_000;

/// The longest request line [`render`] takes, and the longest text in one.
#[cfg(feature = "neural")]
const RENDER_LINE_MAX: usize = 64 * 1024;
#[cfg(feature = "neural")]
const RENDER_TEXT_MAX: usize = 4096;

/// A request to [`render`].
#[cfg(feature = "neural")]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderRequest {
    id: u64,
    #[serde(default)]
    voice: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    lang: Option<String>,
    /// Words per minute; 0: the model's own speed.
    #[serde(default)]
    rate: u32,
    /// Stop making request `id`: nothing more comes for it.
    #[serde(default)]
    cancel: bool,
}

/// The speed a model speaks at for `rate` words per minute (0: its own), as a speaker's.
#[cfg(feature = "neural")]
fn render_speed(rate: u32) -> f32 {
    if rate == 0 {
        1.0
    } else {
        (rate as f32 / twr_core::speech::DEFAULT_WPM as f32).clamp(0.5, 2.0)
    }
}

/// `twr voices render`: each request line `{"id":N,"voice":"kokoro/af_heart","text":"...",
/// "lang":"en","rate":0}` is made a sentence at a time, each sentence one line
/// `{"id":N,"seq":K,"rate":24000,"pcm":"<base64 of 16-bit little-endian mono>"}`, then
/// `{"id":N,"done":true}` (or `{"id":N,"error":"..."}`). `{"id":N,"cancel":true}` stops one.
/// Requests are made in the order they come (one model thread); it ends with its stdin.
#[cfg(feature = "neural")]
pub async fn render(
    synth: twr_neural::synth::Synth,
    input: impl tokio::io::AsyncRead + Unpin,
    output: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
) -> anyhow::Result<()> {
    use base64::Engine as _;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(line) = rx.recv().await {
            let mut text = line.to_string();
            text.push('\n');
            if output.write_all(text.as_bytes()).await.is_err() || output.flush().await.is_err() {
                break;
            }
        }
    });
    let mut running: std::collections::HashMap<u64, tokio::task::AbortHandle> =
        std::collections::HashMap::new();
    let mut lines = tokio::io::BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        running.retain(|_, task| !task.is_finished());
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > RENDER_LINE_MAX {
            let _ = tx.send(json!({"error": "request too long"}));
            continue;
        }
        let req: RenderRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(json!({"error": format!("bad request: {e}")}));
                continue;
            }
        };
        if req.cancel {
            if let Some(task) = running.remove(&req.id) {
                task.abort();
            }
            continue;
        }
        let id = req.id;
        let Some((model, voice)) = twr_neural::voice(&req.voice) else {
            let _ = tx.send(json!({"id": id, "error": format!("voice {:?}: no such voice of twr's", req.voice)}));
            continue;
        };
        if req.text.trim().is_empty() || req.text.chars().count() > RENDER_TEXT_MAX {
            let _ = tx.send(json!({"id": id, "error": "nothing to say, or too much"}));
            continue;
        }
        let mut speech = synth.say(twr_neural::synth::Say {
            model,
            voice,
            lang: req
                .lang
                .filter(|l| twr_core::speech::LANGUAGES.contains(&l.as_str())),
            text: req.text,
            speed: render_speed(req.rate),
        });
        let tx = tx.clone();
        let task = tokio::spawn(async move {
            let mut seq = 0u64;
            while let Some(clip) = speech.clips.recv().await {
                match clip {
                    Ok(clip) => {
                        let samples = twr_neural::audio::resample(
                            &clip.samples,
                            clip.sample_rate,
                            RENDER_RATE,
                        );
                        let mut pcm = Vec::with_capacity(samples.len() * 2);
                        for s in samples {
                            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                            pcm.extend_from_slice(&v.to_le_bytes());
                        }
                        let pcm = base64::engine::general_purpose::STANDARD.encode(pcm);
                        let _ =
                            tx.send(json!({"id": id, "seq": seq, "rate": RENDER_RATE, "pcm": pcm}));
                        seq += 1;
                    }
                    Err(e) => {
                        let _ = tx.send(json!({"id": id, "error": format!("{e:#}")}));
                        return;
                    }
                }
            }
            let _ = tx.send(json!({"id": id, "done": true}));
        });
        running.insert(id, task.abort_handle());
    }
    // Stdin closed: what is still being made is finished and written, then it ends.
    drop(tx);
    let _ = writer.await;
    Ok(())
}

#[cfg(feature = "neural")]
/// What `install` checks of the archive as it comes ([`ArchiveCheck::update`], before each
/// chunk is written) and once it has it all ([`ArchiveCheck::finish`]): no more than its
/// size, then exactly its size and SHA-256.
struct ArchiveCheck {
    spec: &'static twr_neural::ModelSpec,
    got: u64,
    digest: ring::digest::Context,
}

#[cfg(feature = "neural")]
impl ArchiveCheck {
    fn new(spec: &'static twr_neural::ModelSpec) -> ArchiveCheck {
        ArchiveCheck {
            spec,
            got: 0,
            digest: ring::digest::Context::new(&ring::digest::SHA256),
        }
    }

    fn update(&mut self, chunk: &[u8]) -> anyhow::Result<()> {
        self.got += chunk.len() as u64;
        if self.got > self.spec.bytes {
            bail!(
                "{} sent more than the model's {} bytes; nothing was kept",
                self.spec.url(),
                self.spec.bytes
            );
        }
        self.digest.update(chunk);
        Ok(())
    }

    fn finish(self) -> anyhow::Result<()> {
        if self.got != self.spec.bytes {
            bail!(
                "{} did not send the whole model ({} of {} bytes); nothing was kept",
                self.spec.url(),
                self.got,
                self.spec.bytes
            );
        }
        let actual: String = self
            .digest
            .finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != self.spec.sha256 {
            bail!(
                "{} is not the model twr expects: its SHA-256 is {actual}, not {}; nothing \
                 was kept",
                self.spec.url(),
                self.spec.sha256
            );
        }
        Ok(())
    }
}

#[cfg(feature = "neural")]
/// Prints a progress line (`--json`) or a percentage (every 10%) on stderr.
struct Progress {
    json: bool,
    model: &'static str,
    total: u64,
    shown: u64,
    last: std::time::Instant,
}

#[cfg(feature = "neural")]
impl Progress {
    fn update(&mut self, got: u64) {
        let pct = got * 100 / self.total.max(1);
        if self.json {
            if self.last.elapsed() >= std::time::Duration::from_millis(250) || got == self.total {
                self.last = std::time::Instant::now();
                println!(
                    "{}",
                    json!({"type": "progress", "model": self.model, "bytes": got, "total": self.total})
                );
            }
        } else if pct >= self.shown + 10 {
            self.shown = pct - pct % 10;
            eprintln!("twr: {}%", self.shown);
        }
    }

    fn phase(&self, phase: &str) {
        if self.json {
            println!(
                "{}",
                json!({"type": "phase", "model": self.model, "phase": phase})
            );
        } else {
            eprintln!("twr: {phase}");
        }
    }
}

#[cfg(feature = "neural")]
/// Downloads, checks and installs `spec` (see the module documentation).
async fn install(
    spec: &'static twr_neural::ModelSpec,
    data_dir: &Path,
    espeak: &str,
    path: Option<&std::ffi::OsStr>,
    json: bool,
) -> anyhow::Result<()> {
    let source = spec.url();
    let root = twr_neural::models_dir(data_dir);
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    let mut progress = Progress {
        json,
        model: spec.name,
        total: spec.bytes,
        shown: 0,
        last: std::time::Instant::now() - std::time::Duration::from_secs(1),
    };
    if !json {
        eprintln!("twr: downloading {source} ({})", spec.size());
    }
    progress.phase("downloading");
    let client = crate::https_client()
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(60 * 60))
        .build()
        .context("setting up the HTTP client")?;
    let mut resp = client
        .get(&source)
        .send()
        .await
        .with_context(|| format!("downloading {source}"))?;
    if !resp.status().is_success() {
        bail!("downloading {source} failed: HTTP {}", resp.status());
    }
    if let Some(total) = resp.content_length().filter(|n| *n != spec.bytes) {
        bail!(
            "{source} is {total} bytes, not the model's {}; nothing was kept",
            spec.bytes
        );
    }
    // A private file next to the models (same file system as the unpacking): gone if anything
    // fails or the download is stopped.
    let mut part = tempfile::Builder::new()
        .prefix(".download-")
        .suffix(".tar.bz2")
        .tempfile_in(&root)?;
    let mut check = ArchiveCheck::new(spec);
    let download = async {
        use std::io::Write;
        let chunk_wait = std::time::Duration::from_secs(60);
        while let Some(chunk) = tokio::time::timeout(chunk_wait, resp.chunk())
            .await
            .map_err(|_| anyhow!("the download stalled"))?
            .context("downloading the model")?
        {
            check.update(&chunk)?;
            part.write_all(&chunk)?;
            progress.update(check.got);
        }
        anyhow::Ok(())
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("handling SIGTERM")?;
    tokio::select! {
        r = download => r?,
        _ = term.recv() => bail!("download stopped"),
        _ = tokio::signal::ctrl_c() => bail!("download stopped"),
    }
    check.finish()?;
    part.as_file().sync_all().context("writing the model")?;
    progress.phase("installing");
    let espeak = twr_neural::phonemes::find_espeak(espeak, path);
    let data = data_dir.to_path_buf();
    let archive = part.path().to_path_buf();
    let installed = tokio::task::spawn_blocking(move || {
        twr_neural::install::install_archive(&data, spec, &archive, espeak.as_deref())
    })
    .await
    .context("installing the model")??;
    drop(part);
    if json {
        println!(
            "{}",
            json!({"type": "installed", "model": spec.name, "rtf": installed.rtf, "fast": installed.fast()})
        );
    } else {
        let speed = match installed.fast() {
            Some(true) => "fast enough on this machine",
            Some(false) => "slow on this machine: it may fall behind",
            None => "not timed (it needs espeak-ng)",
        };
        println!("voices: {} installed, {speed}", spec.title);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAY: &str = "\
Albert              en_US    # Hello! My name is Albert.
Ava (Premium)       en_US    # Hello! My name is Ava.
Ava (Enhanced)      en_US    # Hello! My name is Ava.
Eddy (Spanish (Spain)) es_ES    # ¡Hola! Me llamo Eddy.
Mónica              es_ES    # ¡Hola! Me llamo Mónica.
Mónica              es_ES    # ¡Hola! Me llamo Mónica.
Marisol (Premium)   es_ES    # ¡Hola! Me llamo Marisol.
Samantha            en_US    # Hello! My name is Samantha.
Thomas              fr_FR    # Bonjour, je m’appelle Thomas.
not a voice line
";

    #[test]
    fn macos_voices_are_listed_best_first() {
        let v = parse_say_voices(SAY, Some(twr_core::speech::LANGUAGES));
        let ids: Vec<_> = v.iter().map(|r| (r.id.as_str(), r.quality)).collect();
        assert_eq!(
            ids,
            vec![
                ("Ava (Premium)", "premium"),
                ("Ava (Enhanced)", "enhanced"),
                ("Samantha", "default"),
                ("Marisol (Premium)", "premium"),
                ("Eddy (Spanish (Spain))", "default"),
                ("Mónica", "default"),
            ]
        );
        assert_eq!(v[3].locale, "es-ES");
        assert_eq!(v[3].languages, vec!["es"]);
        assert_eq!(parse_say_voices(SAY, None).len(), 7);
    }

    #[cfg(feature = "neural")]
    #[test]
    fn the_archive_is_checked_as_it_comes() {
        let spec = twr_neural::model("piper-es").unwrap();
        let mut c = ArchiveCheck::new(spec);
        c.update(&[0; 10]).unwrap();
        let e = c.finish().unwrap_err().to_string();
        assert!(e.contains("10 of 67207890 bytes"), "{e}");
        let mut c = ArchiveCheck::new(spec);
        c.got = spec.bytes;
        assert!(c.update(&[0]).is_err());
    }

    /// What `render` answers without a model: an error per request, by its id, and nothing
    /// for a request it can't read but a line saying so.
    #[cfg(feature = "neural")]
    #[tokio::test]
    async fn render_answers_each_request_by_its_id() {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let synth = twr_neural::synth::Synth::start(dir.path().to_path_buf(), None);
        let (mut input, stdin) = tokio::io::duplex(64 * 1024);
        let (stdout, mut output) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(render(synth, stdin, stdout));
        input
            .write_all(
                b"{\"id\":1,\"voice\":\"nope/x\",\"text\":\"Hi.\"}\n\
                  not json\n\
                  {\"id\":2,\"voice\":\"supertonic/F1\",\"text\":\"  \"}\n\
                  {\"id\":3,\"voice\":\"supertonic/F1\",\"text\":\"Hello.\",\"lang\":\"en\"}\n\
                  {\"id\":4,\"cancel\":true}\n",
            )
            .await
            .unwrap();
        drop(input);
        task.await.unwrap().unwrap();
        let mut text = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut output, &mut text)
            .await
            .unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let by = |id: u64| lines.iter().find(|l| l["id"] == id).cloned();
        assert!(by(1).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("no such voice"));
        assert!(lines
            .iter()
            .any(|l| l["id"].is_null() && l["error"].as_str().unwrap().starts_with("bad request")));
        assert!(by(2).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("nothing to say"));
        // Not installed here: said by its id, never as audio.
        assert!(by(3).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("not installed"));
        assert!(by(4).is_none(), "a cancel answers nothing");
        assert!(lines.iter().all(|l| l.get("pcm").is_none()));
    }

    #[test]
    fn sample_sentences_follow_the_voice() {
        #[cfg(feature = "neural")]
        assert!(sample_text("kokoro/ef_dora").starts_with("Hola"));
        assert!(sample_text("Marisol (Premium)").starts_with("Hola"));
        assert!(sample_text("Eddy (Spanish (Spain))").starts_with("Hola"));
        assert!(sample_text("Ava (Premium)").starts_with("Hi"));
        assert!(sample_text("supertonic/F1").starts_with("Hi"));
    }
}
