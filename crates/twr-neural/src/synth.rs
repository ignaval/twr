//! Making speech: [`Synth`] owns a thread that loads the models it is asked for and turns a
//! chunk of text into audio a sentence at a time ([`Clip`]s on a channel), so a speaker plays
//! the first sentence while the next is made. Dropping the receiver, or setting the request's
//! cancel flag, stops it after the sentence being made. A model nobody asked for in [`IDLE`]
//! is unloaded (Kokoro holds about 800 MB).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig,
    OfflineTtsModelConfig, OfflineTtsSupertonicModelConfig, OfflineTtsVitsModelConfig,
};
use tokio::sync::mpsc as tmpsc;

use crate::phonemes::{self, Espeak};
use crate::{Family, ModelSpec, VoiceSpec};

/// How long a loaded model stays loaded unused.
pub const IDLE: Duration = Duration::from_secs(120);

/// One sentence's audio: mono samples in -1.0..1.0.
#[derive(Debug, Clone)]
pub struct Clip {
    /// The samples.
    pub samples: Vec<f32>,
    /// Samples per second.
    pub sample_rate: u32,
}

impl Clip {
    /// How long it plays, in seconds.
    pub fn secs(&self) -> f32 {
        self.samples.len() as f32 / self.sample_rate.max(1) as f32
    }

    /// As a 16-bit PCM WAV file, at `volume` (0.0 to 1.0).
    pub fn wav(&self, volume: f32) -> Vec<u8> {
        let data_len = (self.samples.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&(self.sample_rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        let volume = volume.clamp(0.0, 1.0);
        for s in &self.samples {
            let v = (s * volume).clamp(-1.0, 1.0);
            out.extend_from_slice(&((v * i16::MAX as f32) as i16).to_le_bytes());
        }
        out
    }
}

/// What to say.
#[derive(Debug, Clone)]
pub struct Say {
    /// The model.
    pub model: &'static ModelSpec,
    /// Its voice.
    pub voice: &'static VoiceSpec,
    /// The message's language, when known: Supertonic reads it so (else the voice's own).
    pub lang: Option<String>,
    /// The text: one chunk (`twr_core::speech::CHUNK_MAX` characters at most).
    pub text: String,
    /// The speed: 1.0 is the model's own.
    pub speed: f32,
}

struct Request {
    say: Say,
    cancel: Arc<AtomicBool>,
    out: tmpsc::UnboundedSender<anyhow::Result<Clip>>,
}

/// A request being made: its clips arrive on `clips`; [`Speech::cancel`] (or dropping it)
/// stops it after the sentence being made.
pub struct Speech {
    /// One clip per sentence, then the end of the channel; an error ends it.
    pub clips: tmpsc::UnboundedReceiver<anyhow::Result<Clip>>,
    cancel: Arc<AtomicBool>,
}

impl Speech {
    /// Stop making the rest.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

impl Drop for Speech {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The models' thread (see the module documentation). Cheap to clone; the thread ends with
/// the last clone.
#[derive(Clone)]
pub struct Synth {
    tx: mpsc::Sender<Request>,
}

impl std::fmt::Debug for Synth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Synth")
    }
}

impl Synth {
    /// A thread for the models installed under `data_dir`, spelling with `espeak` (`None`:
    /// the Kokoro and Piper voices fail, saying what to install).
    pub fn start(data_dir: PathBuf, espeak: Option<PathBuf>) -> Synth {
        let (tx, rx) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("twr-neural".into())
            .spawn(move || {
                let mut worker = Worker {
                    data_dir,
                    espeak: espeak.map(|program| Espeak { program }),
                    loaded: BTreeMap::new(),
                };
                loop {
                    match rx.recv_timeout(IDLE) {
                        Ok(r) => worker.serve(r),
                        Err(mpsc::RecvTimeoutError::Timeout) => worker.loaded.clear(),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("starting the twr-neural thread");
        Synth { tx }
    }

    /// Starts saying `say`: its clips come on the returned [`Speech`].
    pub fn say(&self, say: Say) -> Speech {
        let (out, clips) = tmpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let req = Request {
            say,
            cancel: cancel.clone(),
            out,
        };
        if let Err(mpsc::SendError(req)) = self.tx.send(req) {
            let _ = req.out.send(Err(anyhow!("the voice thread is gone")));
        }
        Speech { clips, cancel }
    }
}

/// A model loaded, with the characters its tokens know (phonemes outside them are dropped
/// before they reach it).
pub struct Loaded {
    tts: OfflineTts,
    tokens: HashSet<char>,
}

struct Worker {
    data_dir: PathBuf,
    espeak: Option<Espeak>,
    loaded: BTreeMap<&'static str, Loaded>,
}

impl Worker {
    fn serve(&mut self, r: Request) {
        if let Err(e) = self.say(&r) {
            let _ = r.out.send(Err(e));
        }
    }

    fn say(&mut self, r: &Request) -> anyhow::Result<()> {
        let Say {
            model,
            voice,
            lang,
            text,
            speed,
        } = &r.say;
        let espeak = match model.family {
            Family::Supertonic => None,
            _ => Some(
                self.espeak
                    .clone()
                    .ok_or_else(|| anyhow!("{}: {}", model.title, phonemes::espeak_hint()))?,
            ),
        };
        if !self.loaded.contains_key(model.name) {
            let loaded = load(model, &model.path(&self.data_dir))?;
            self.loaded.insert(model.name, loaded);
        }
        let loaded = &self.loaded[model.name];
        let lang = lang
            .as_deref()
            .filter(|l| voice.languages.contains(l))
            .unwrap_or(voice.languages[0]);
        for sentence in phonemes::sentences(text) {
            if r.cancel.load(Ordering::SeqCst) || r.out.is_closed() {
                return Ok(());
            }
            let clip = generate(
                model,
                voice,
                lang,
                &sentence,
                *speed,
                espeak.as_ref(),
                loaded,
            )?;
            if let Some(clip) = clip {
                if r.out.send(Ok(clip)).is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// One sentence's clip; `None` when there was nothing to say in it (only marks).
fn generate(
    model: &ModelSpec,
    voice: &VoiceSpec,
    lang: &str,
    sentence: &str,
    speed: f32,
    espeak: Option<&Espeak>,
    loaded: &Loaded,
) -> anyhow::Result<Option<Clip>> {
    let mut cfg = GenerationConfig {
        sid: voice.sid,
        speed: speed.clamp(0.5, 2.0),
        ..Default::default()
    };
    let text = match (model.family, espeak) {
        (Family::Supertonic, _) => {
            let mut extra = std::collections::HashMap::new();
            extra.insert("lang".to_string(), serde_json::json!(lang));
            cfg.extra = Some(extra);
            sentence.to_string()
        }
        (family, Some(espeak)) => {
            let spelled = espeak.sentence(voice.espeak, sentence, |ipa| match family {
                Family::Kokoro => phonemes::for_kokoro(ipa, voice.espeak),
                _ => phonemes::for_piper(ipa),
            })?;
            let codepoints: Vec<i32> = spelled
                .chars()
                .filter(|c| loaded.tokens.contains(c))
                .map(|c| c as i32)
                .collect();
            let sound =
                |c: &i32| char::from_u32(*c as u32).is_some_and(|c| !" ,.;:!?…".contains(c));
            if !codepoints.iter().any(sound) {
                return Ok(None);
            }
            cfg.phoneme_codepoints = Some(vec![codepoints]);
            String::new()
        }
        (_, None) => bail!("{}: {}", model.title, phonemes::espeak_hint()),
    };
    if text.trim().is_empty() && cfg.phoneme_codepoints.is_none() {
        return Ok(None);
    }
    let audio = loaded
        .tts
        .generate_with_config(&text, &cfg, None::<fn(&[f32], f32) -> bool>)
        .ok_or_else(|| anyhow!("{} could not say that", model.title))?;
    let samples = audio.samples().to_vec();
    if samples.is_empty() {
        return Ok(None);
    }
    Ok(Some(Clip {
        samples,
        sample_rate: audio.sample_rate().max(1) as u32,
    }))
}

/// Loads `model` from `dir` (installed: [`crate::install::installed`]).
pub fn load(model: &ModelSpec, dir: &Path) -> anyhow::Result<Loaded> {
    for f in model.files {
        if !dir.join(f).is_file() {
            bail!(
                "{} is not installed (no {}): `twr voices install {}`",
                model.title,
                dir.join(f).display(),
                model.name
            );
        }
    }
    let p = |f: &str| Some(dir.join(f).to_string_lossy().into_owned());
    let mut cfg = OfflineTtsModelConfig {
        num_threads: crate::threads(),
        ..Default::default()
    };
    match model.family {
        Family::Kokoro => {
            cfg.kokoro = OfflineTtsKokoroModelConfig {
                model: p(model.files[0]),
                voices: p("voices.bin"),
                tokens: p("tokens.txt"),
                ..Default::default()
            }
        }
        Family::Piper => {
            cfg.vits = OfflineTtsVitsModelConfig {
                model: p(model.files[0]),
                tokens: p("tokens.txt"),
                ..Default::default()
            }
        }
        Family::Supertonic => {
            cfg.supertonic = OfflineTtsSupertonicModelConfig {
                duration_predictor: p("duration_predictor.int8.onnx"),
                text_encoder: p("text_encoder.int8.onnx"),
                vector_estimator: p("vector_estimator.int8.onnx"),
                vocoder: p("vocoder.int8.onnx"),
                tts_json: p("tts.json"),
                unicode_indexer: p("unicode_indexer.bin"),
                voice_style: p("voice.bin"),
            }
        }
    }
    let tokens = match model.family {
        Family::Supertonic => HashSet::new(),
        _ => read_tokens(&dir.join("tokens.txt"))?,
    };
    let tts = OfflineTts::create(&OfflineTtsConfig {
        model: cfg,
        ..Default::default()
    })
    .ok_or_else(|| anyhow!("{} did not load from {}", model.title, dir.display()))?;
    Ok(Loaded { tts, tokens })
}

/// The characters of a model's `tokens.txt` (`<symbol> <id>` a line; the space symbol is a
/// line starting with a space).
fn read_tokens(path: &Path) -> anyhow::Result<HashSet<char>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut set = HashSet::new();
    for line in text.lines() {
        let Some((sym, _id)) = line.rsplit_once(' ') else {
            continue;
        };
        let mut chars = sym.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => {
                set.insert(c);
            }
            // The space token's line is " <id>".
            (None, _) => {
                set.insert(' ');
            }
            _ => {}
        }
    }
    Ok(set)
}

/// Says `text` with `voice` of the model in `dir` now, on this thread, and returns the audio
/// and how long making it took ([`crate::install::bench`]).
pub fn say_now(
    dir: &Path,
    espeak: Option<&Path>,
    model: &'static ModelSpec,
    voice: &'static VoiceSpec,
    text: &str,
) -> anyhow::Result<(Vec<Clip>, Duration)> {
    let loaded = load(model, dir)?;
    let espeak = espeak.map(|p| Espeak {
        program: p.to_path_buf(),
    });
    let lang = voice.languages[0];
    let start = std::time::Instant::now();
    let mut clips = Vec::new();
    for sentence in phonemes::sentences(text) {
        if let Some(c) = generate(model, voice, lang, &sentence, 1.0, espeak.as_ref(), &loaded)? {
            clips.push(c);
        }
    }
    Ok((clips, start.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_is_a_wav_file() {
        let c = Clip {
            samples: vec![0.0, 0.5, -0.5, 1.5],
            sample_rate: 24_000,
        };
        let w = c.wav(1.0);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 24_000);
        assert_eq!(w.len(), 44 + 8);
        // Clamped, and scaled by the volume.
        assert_eq!(i16::from_le_bytes(w[50..52].try_into().unwrap()), i16::MAX);
        let half = c.wav(0.5);
        assert_eq!(
            i16::from_le_bytes(half[46..48].try_into().unwrap()),
            i16::MAX / 4
        );
        assert!((c.secs() - 4.0 / 24_000.0).abs() < 1e-6);
    }

    #[test]
    fn tokens_files_read_with_their_space() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tokens.txt");
        std::fs::write(&p, "$ 0\n; 1\n  16\nɑ 69\nab 3\n").unwrap();
        let t = read_tokens(&p).unwrap();
        assert!(t.contains(&'$') && t.contains(&';') && t.contains(&' ') && t.contains(&'ɑ'));
        assert_eq!(t.len(), 4);
    }

    #[test]
    fn a_missing_model_says_how_to_install_it() {
        let dir = tempfile::tempdir().unwrap();
        let m = crate::model("kokoro").unwrap();
        let e = load(m, &m.path(dir.path())).err().unwrap().to_string();
        assert!(e.contains("twr voices install kokoro"), "{e}");
    }
}
