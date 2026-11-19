//! twr's own voices in a speaker: [`NeuralEngine`] says a chunk with a local neural voice
//! (`twr_neural`, a voice named `<model>/<voice>`), a sentence at a time through twr's own
//! audio output ([`twr_neural::audio::Output`]); [`AutoEngine`] is `[speech] engine = "auto"`:
//! each chunk goes to twr's own engine when its voice is one of twr's, else to the system's
//! (`say`, `spd-say`), so English can be a macOS voice and Spanish a Kokoro one.
//!
//! - **Sentences.** The model makes a chunk's sentences one after another while the first
//!   plays ([`twr_neural::synth::Synth`]), each queued to the output as it is made, so they
//!   follow each other with no gap. Dropping the chunk's future silences the output at once
//!   and stops what is still being made. Pause and resume are in place.
//! - **The next chunk.** [`Engine::prepare`] starts making the chunk the player says next while
//!   this one plays, so it starts at once.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use twr_neural::audio::Output;
use twr_neural::synth::{Say, Speech, Synth};

use crate::engine::{Engine, Prefs, QuietFuture, SpeakFuture};

/// Why twr's voices can't play on a machine without an output device.
pub const NO_OUTPUT: &str = "no audio output device for twr's voices";

/// The speed a model speaks at for `rate` words per minute (0: its own).
fn speed(rate: u32) -> f32 {
    if rate == 0 {
        1.0
    } else {
        (rate as f32 / twr_core::speech::DEFAULT_WPM as f32).clamp(0.5, 2.0)
    }
}

/// What a voice name asks of twr's models, or why it can't be said.
fn request(chunk: &str, prefs: &Prefs) -> anyhow::Result<Say> {
    let name = prefs.voice.as_deref().unwrap_or_default();
    let (model, voice) = twr_neural::voice(name)
        .ok_or_else(|| anyhow!("voice {name:?}: no such voice of twr's (`twr voices list`)"))?;
    Ok(Say {
        model,
        voice,
        lang: prefs.lang.clone(),
        text: chunk.to_string(),
        speed: speed(prefs.rate),
    })
}

/// A chunk being made ahead ([`Engine::prepare`]), to be said if it is the next one asked.
struct Ahead {
    text: String,
    voice: Option<String>,
    speech: Speech,
}

/// twr's own voices (see the module documentation).
pub struct NeuralEngine {
    synth: Synth,
    output: Output,
    ahead: Option<Ahead>,
}

impl NeuralEngine {
    /// The models installed under `data_dir`, spelled with `espeak` (`None`: only
    /// Supertonic's voices speak), played on the default output device.
    pub fn new(data_dir: &Path, espeak: Option<PathBuf>) -> NeuralEngine {
        NeuralEngine {
            synth: Synth::start(data_dir.to_path_buf(), espeak),
            output: Output::new(),
            ahead: None,
        }
    }

    fn speech(&mut self, chunk: &str, prefs: &Prefs) -> anyhow::Result<Speech> {
        if let Some(a) = self.ahead.take() {
            if a.text == chunk && a.voice == prefs.voice {
                return Ok(a.speech);
            }
        }
        Ok(self.synth.say(request(chunk, prefs)?))
    }
}

impl Engine for NeuralEngine {
    fn name(&self) -> &str {
        "neural"
    }

    fn speak(&mut self, chunk: &str, prefs: &Prefs) -> SpeakFuture {
        let speech = self.speech(chunk, prefs);
        let output = self.output.clone();
        let volume = prefs.volume;
        Box::pin(async move { play(speech?, output, volume).await })
    }

    fn pause(&mut self) -> bool {
        self.output.pause(true);
        true
    }

    fn resume(&mut self) -> bool {
        self.output.pause(false);
        true
    }

    fn after_stop(&mut self) -> QuietFuture {
        self.output.pause(false);
        self.output.clear();
        Box::pin(async {})
    }

    fn prepare(&mut self, next: &str, prefs: &Prefs) {
        let Ok(say) = request(next, prefs) else {
            return;
        };
        self.ahead = Some(Ahead {
            text: next.to_string(),
            voice: prefs.voice.clone(),
            speech: self.synth.say(say),
        });
    }
}

/// Silences the output when a chunk's future goes before it was all played.
struct Silence(Output, bool);

impl Drop for Silence {
    fn drop(&mut self) {
        if !self.1 {
            self.0.clear();
        }
    }
}

/// Queues `speech`'s clips as they come, then waits until the device played the last.
async fn play(mut speech: Speech, output: Output, volume: f32) -> anyhow::Result<()> {
    let mut guard = Silence(output.clone(), false);
    let mut end = output.played();
    while let Some(clip) = speech.clips.recv().await {
        end = output.queue(&clip?, volume)?;
    }
    output.until(end).await?;
    guard.1 = true;
    Ok(())
}

/// Which engine said the chunk being said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    System,
    Neural,
}

/// `auto`: twr's own voices for theirs, the system's engine for any other (see the module
/// documentation). Either may be missing: a voice that needs it fails with why.
pub struct AutoEngine {
    system: Option<Box<dyn Engine>>,
    neural: Option<NeuralEngine>,
    /// Why there is no neural engine (no output device), for its voices' errors.
    no_neural: &'static str,
    active: Option<Side>,
    /// What the engines' names were reported as.
    name: String,
}

impl AutoEngine {
    /// The system's engine (`None`: none on this machine) and twr's (`None` with why).
    pub fn new(
        system: Option<Box<dyn Engine>>,
        neural: Result<NeuralEngine, &'static str>,
    ) -> AutoEngine {
        let name = match &system {
            Some(s) => s.name().to_string(),
            None => "neural".to_string(),
        };
        let (neural, no_neural) = match neural {
            Ok(n) => (Some(n), ""),
            Err(why) => (None, why),
        };
        AutoEngine {
            system,
            neural,
            no_neural,
            active: None,
            name,
        }
    }

    fn side(prefs: &Prefs) -> Side {
        match prefs.voice.as_deref() {
            Some(v) if twr_neural::is_neural(v) => Side::Neural,
            _ => Side::System,
        }
    }

    fn engine(&mut self, side: Side) -> Option<&mut (dyn Engine + 'static)> {
        match side {
            Side::System => self.system.as_deref_mut(),
            Side::Neural => self
                .neural
                .as_mut()
                .map(|n| n as &mut (dyn Engine + 'static)),
        }
    }
}

impl Engine for AutoEngine {
    fn name(&self) -> &str {
        &self.name
    }

    fn speak(&mut self, chunk: &str, prefs: &Prefs) -> SpeakFuture {
        let side = Self::side(prefs);
        self.active = Some(side);
        let no_neural = self.no_neural;
        match self.engine(side) {
            Some(e) => e.speak(chunk, prefs),
            None => {
                let err = match side {
                    Side::Neural => anyhow!("{no_neural}"),
                    Side::System => anyhow!(crate::engine::NO_ENGINE),
                };
                Box::pin(async move { Err(err) })
            }
        }
    }

    fn prepare(&mut self, next: &str, prefs: &Prefs) {
        if let Some(e) = self.engine(Self::side(prefs)) {
            e.prepare(next, prefs);
        }
    }

    fn pause(&mut self) -> bool {
        match self.active {
            Some(side) => self.engine(side).is_some_and(|e| e.pause()),
            None => false,
        }
    }

    fn resume(&mut self) -> bool {
        match self.active {
            Some(side) => self.engine(side).is_some_and(|e| e.resume()),
            None => false,
        }
    }

    fn after_stop(&mut self) -> QuietFuture {
        match self.active.and_then(|side| self.engine(side)) {
            Some(e) => e.after_stop(),
            None => Box::pin(async {}),
        }
    }
}

/// Whether any of twr's models is installed under `data_dir`.
pub fn any_installed(data_dir: &Path) -> bool {
    twr_neural::MODELS
        .iter()
        .any(|m| twr_neural::install::installed(data_dir, m).is_some())
}

/// The neural engine for `data_dir` with `espeak`, or why there is none (no output device).
pub fn neural_engine(
    data_dir: &Path,
    espeak: &str,
    path: Option<&OsStr>,
) -> Result<NeuralEngine, &'static str> {
    if !twr_neural::audio::has_output() {
        return Err(NO_OUTPUT);
    }
    let espeak = twr_neural::phonemes::find_espeak(espeak, path);
    Ok(NeuralEngine::new(data_dir, espeak))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{FakeEngine, FakeEvent};

    fn prefs(voice: &str) -> Prefs {
        Prefs {
            voice: Some(voice.to_string()),
            ..Prefs::default()
        }
    }

    #[tokio::test]
    async fn auto_sends_twrs_voices_to_its_own_engine_and_others_to_the_system() {
        let (system, h) = FakeEngine::timed(std::time::Duration::from_millis(1));
        let mut auto = AutoEngine::new(Some(Box::new(system)), Err(NO_OUTPUT));
        assert_eq!(auto.name(), "fake");
        auto.speak("Hello.", &prefs("Ava (Premium)")).await.unwrap();
        assert_eq!(h.spoken(), vec!["Hello."]);
        // twr's voice without a neural engine: why, and the system never gets it.
        let e = auto
            .speak("Hola.", &prefs("kokoro/ef_dora"))
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), NO_OUTPUT);
        assert_eq!(h.spoken(), vec!["Hello."]);
        assert!(h
            .events()
            .iter()
            .all(|e| !matches!(e, FakeEvent::Speak { chunk, .. } if chunk == "Hola.")));
    }

    #[tokio::test]
    async fn auto_without_a_system_engine_says_so_for_its_voices() {
        let dir = tempfile::tempdir().unwrap();
        let neural = NeuralEngine::new(dir.path(), None);
        let mut auto = AutoEngine::new(None, Ok(neural));
        assert_eq!(auto.name(), "neural");
        let e = auto.speak("Hello.", &prefs("Ava")).await.unwrap_err();
        assert_eq!(e.to_string(), crate::engine::NO_ENGINE);
        // An unknown voice of twr's, and a model that isn't installed: what to do.
        let e = auto
            .speak("Hello.", &prefs("kokoro/nobody"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("twr voices list"), "{e}");
        let e = auto
            .speak("Hello.", &prefs("supertonic/F1"))
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("twr voices install supertonic"),
            "{e}"
        );
    }

    #[test]
    fn rates_become_speeds() {
        assert_eq!(speed(0), 1.0);
        assert_eq!(speed(175), 1.0);
        assert_eq!(speed(500), 2.0);
        assert_eq!(speed(80), 0.5);
    }
}
