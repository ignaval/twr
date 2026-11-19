//! twr's local neural voices (`docs/read-aloud.md#local-voices`): models that run on the
//! machine's CPU through sherpa-onnx, downloaded once, used by `twr speak`'s `auto` engine for
//! any voice named `<model>/<voice>`.
//!
//! - **Catalog** ([`MODELS`]): each model pinned to one archive of sherpa-onnx's `tts-models`
//!   release, by its size and SHA-256, and the files it must have once unpacked. Nothing else
//!   is ever downloaded or loaded.
//! - **Spelling** ([`phonemes`]): Kokoro and Piper read phonemes, not text. sherpa-onnx 2.x
//!   left espeak-ng (GPL) out, so twr runs the `espeak-ng` program for them: the text goes to
//!   its stdin, never into an argument. Supertonic reads text and needs no program.
//! - **Speech** ([`synth::Synth`]): one thread owns the loaded models and makes a message's
//!   audio a sentence at a time, so the first sentence plays while the next is made. A model
//!   unused for [`synth::IDLE`] is unloaded.
//! - **Playing** ([`audio::Output`]): one output stream the sentences are queued to, so they
//!   follow each other with no gap; it pauses in place and stops at once.
//! - **Installing** ([`install`]): an archive already checked against the catalog is unpacked
//!   with the system's `tar`, its files checked, timed ([`install::bench`]) and renamed into
//!   place in one step; a half-unpacked model never counts as installed.

pub mod audio;
pub mod install;
pub mod phonemes;
pub mod synth;

use std::path::{Path, PathBuf};

use serde::Serialize;

/// The kinds of models, which differ in their files and in what they read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    /// Kokoro 82M: phonemes (espeak-ng), 24 kHz, a voice per speaker id.
    Kokoro,
    /// Supertonic 3: text in any of its languages, 44.1 kHz, ten voices.
    Supertonic,
    /// Piper (VITS): phonemes (espeak-ng), 22.05 kHz, one voice.
    Piper,
}

/// A model of the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    /// Its name, the first part of its voices' names (`kokoro`).
    pub name: &'static str,
    /// What `twr voices list` calls it.
    pub title: &'static str,
    /// Its family.
    pub family: Family,
    /// The archive's file name in the release ([`ModelSpec::url`]).
    pub archive: &'static str,
    /// The archive's size, in bytes.
    pub bytes: u64,
    /// The archive's SHA-256, in lowercase hex.
    pub sha256: &'static str,
    /// The directory the archive unpacks to (its only top-level entry).
    pub dir: &'static str,
    /// The files the model needs, relative to its directory.
    pub files: &'static [&'static str],
    /// Its licence, as its model card states it.
    pub license: &'static str,
    /// Its voices.
    pub voices: &'static [VoiceSpec],
}

/// A voice of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceSpec {
    /// Its name within the model (`af_heart`): the voice is `<model>/<name>`.
    pub name: &'static str,
    /// What a list shows (`Heart`).
    pub title: &'static str,
    /// The languages it reads well (`twr_core::speech::LANGUAGES`), its own first.
    pub languages: &'static [&'static str],
    /// Its accent, as a BCP-47 tag (`en-US`).
    pub locale: &'static str,
    /// The espeak-ng voice that spells words out for it (Kokoro, Piper); empty for
    /// Supertonic.
    pub espeak: &'static str,
    /// Its speaker id in the model.
    pub sid: i32,
}

const RELEASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models";

macro_rules! kokoro {
    ($name:literal, $title:literal, $lang:literal, $locale:literal, $espeak:literal, $sid:literal) => {
        VoiceSpec {
            name: $name,
            title: $title,
            languages: &[$lang],
            locale: $locale,
            espeak: $espeak,
            sid: $sid,
        }
    };
}

macro_rules! supertonic {
    ($name:literal, $sid:literal) => {
        VoiceSpec {
            name: $name,
            title: $name,
            languages: &["en", "es"],
            locale: "mul",
            espeak: "",
            sid: $sid,
        }
    };
}

/// The models twr knows, the best sounding first. Sizes and hashes are those of the release's
/// assets (GitHub's own `sha256:` digests match).
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        name: "kokoro",
        title: "Kokoro",
        family: Family::Kokoro,
        archive: "kokoro-multi-lang-v1_0.tar.bz2",
        bytes: 349_886_476,
        sha256: "60703707bc05ccd48d60e1eb723f38b07d6e474795a0b6a588d7220400e0f7a4",
        dir: "kokoro-multi-lang-v1_0",
        files: &["model.onnx", "voices.bin", "tokens.txt"],
        license: "Apache-2.0",
        // Speaker ids of kokoro-multi-lang-v1_0's `voices.bin` (its `speaker2id`).
        voices: &[
            kokoro!("af_heart", "Heart", "en", "en-US", "en-us", 3),
            kokoro!("af_bella", "Bella", "en", "en-US", "en-us", 2),
            kokoro!("af_nicole", "Nicole", "en", "en-US", "en-us", 6),
            kokoro!("am_michael", "Michael", "en", "en-US", "en-us", 16),
            kokoro!("am_fenrir", "Fenrir", "en", "en-US", "en-us", 14),
            kokoro!("am_puck", "Puck", "en", "en-US", "en-us", 18),
            kokoro!("bf_emma", "Emma", "en", "en-GB", "en-gb", 21),
            kokoro!("bm_george", "George", "en", "en-GB", "en-gb", 26),
            kokoro!("ef_dora", "Dora", "es", "es-ES", "es", 28),
            kokoro!("em_alex", "Alex", "es", "es-ES", "es", 29),
            kokoro!("em_santa", "Santa", "es", "es-ES", "es", 53),
        ],
    },
    ModelSpec {
        name: "supertonic",
        title: "Supertonic 3",
        family: Family::Supertonic,
        archive: "sherpa-onnx-supertonic-3-tts-int8-2026-05-11.tar.bz2",
        bytes: 128_774_318,
        sha256: "82fa96f91c4ef8abaae3a14a3f4153facf88bed821d1f7331cec2700f432c427",
        dir: "sherpa-onnx-supertonic-3-tts-int8-2026-05-11",
        files: &[
            "duration_predictor.int8.onnx",
            "text_encoder.int8.onnx",
            "vector_estimator.int8.onnx",
            "vocoder.int8.onnx",
            "tts.json",
            "unicode_indexer.bin",
            "voice.bin",
        ],
        license: "OpenRAIL-M (model), MIT (code)",
        // `voice.bin`'s ten styles, upstream's F1-F5 then M1-M5.
        voices: &[
            supertonic!("F1", 0),
            supertonic!("F2", 1),
            supertonic!("F3", 2),
            supertonic!("F4", 3),
            supertonic!("F5", 4),
            supertonic!("M1", 5),
            supertonic!("M2", 6),
            supertonic!("M3", 7),
            supertonic!("M4", 8),
            supertonic!("M5", 9),
        ],
    },
    ModelSpec {
        name: "piper-en",
        title: "Piper (English)",
        family: Family::Piper,
        archive: "vits-piper-en_US-lessac-medium.tar.bz2",
        bytes: 67_230_653,
        sha256: "9e3febfacf0abf4270172d2958bcec246032b7e88efc2720840cc80c93de334e",
        dir: "vits-piper-en_US-lessac-medium",
        files: &["en_US-lessac-medium.onnx", "tokens.txt"],
        license: "MIT (model card)",
        voices: &[kokoro!("lessac", "Lessac", "en", "en-US", "en-us", 0)],
    },
    ModelSpec {
        name: "piper-es",
        title: "Piper (Spanish)",
        family: Family::Piper,
        archive: "vits-piper-es_MX-claude-high.tar.bz2",
        bytes: 67_207_890,
        sha256: "ec33fb689c248fe64810aab564cba97babf0f506672cfd404928d46e751a4721",
        dir: "vits-piper-es_MX-claude-high",
        files: &["es_MX-claude-high.onnx", "tokens.txt"],
        license: "MIT (model card)",
        voices: &[kokoro!("claude", "Claude", "es", "es-MX", "es-419", 0)],
    },
];

impl ModelSpec {
    /// The archive's URL.
    pub fn url(&self) -> String {
        format!("{RELEASE}/{}", self.archive)
    }

    /// Where it is installed under `data_dir`.
    pub fn path(&self, data_dir: &Path) -> PathBuf {
        models_dir(data_dir).join(self.name)
    }

    /// The archive's size for people (`350 MB`).
    pub fn size(&self) -> String {
        format!("{} MB", (self.bytes + 500_000) / 1_000_000)
    }
}

/// The directory of the installed models under twr's data directory.
pub fn models_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("speech")
}

/// The model of the catalog named `name`.
pub fn model(name: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.name == name)
}

/// The model and voice a voice name (`kokoro/af_heart`) names, when it is one of the
/// catalog's. Any other name (`Ava (Premium)`) is the system's.
pub fn voice(name: &str) -> Option<(&'static ModelSpec, &'static VoiceSpec)> {
    let (m, v) = name.split_once('/')?;
    let model = model(m)?;
    let voice = model.voices.iter().find(|x| x.name == v)?;
    Some((model, voice))
}

/// Whether `name` looks like a voice of twr's own (`<model>/<voice>` of a model of the
/// catalog), known voice or not: such a name never goes to the system's engine.
pub fn is_neural(name: &str) -> bool {
    name.split_once('/')
        .is_some_and(|(m, _)| model(m).is_some())
}

/// The full name of `voice` of `model` (`kokoro/af_heart`).
pub fn voice_name(model: &ModelSpec, voice: &VoiceSpec) -> String {
    format!("{}/{}", model.name, voice.name)
}

/// How many threads a model runs on: up to four (more barely helps a model this small, and
/// leaves the machine to the rest of its work).
pub fn threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(2) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_consistent() {
        let mut names = std::collections::BTreeSet::new();
        for m in MODELS {
            assert!(names.insert(m.name), "{} twice", m.name);
            assert!(m.url().starts_with(RELEASE) && m.url().ends_with(".tar.bz2"));
            assert!(
                m.url().contains(m.dir),
                "{}: its archive unpacks to {}",
                m.name,
                m.dir
            );
            assert_eq!(m.sha256.len(), 64);
            assert!(m
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            assert!(!m.files.is_empty() && !m.voices.is_empty());
            let mut voices = std::collections::BTreeSet::new();
            for v in m.voices {
                assert!(voices.insert(v.name), "{}/{} twice", m.name, v.name);
                let name = voice_name(m, v);
                assert_eq!(voice(&name), Some((m, v)));
                assert!(is_neural(&name));
                assert!(!v.languages.is_empty());
                for l in v.languages {
                    assert!(["en", "es"].contains(l), "{name}: {l}");
                }
                assert_eq!(
                    v.espeak.is_empty(),
                    m.family == Family::Supertonic,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn system_voices_are_not_neural() {
        assert!(voice("Ava (Premium)").is_none());
        assert!(!is_neural("Ava (Premium)"));
        assert!(!is_neural("Eddy (Spanish (Spain))"));
        // A model's name with an unknown voice is still twr's: never handed to `say`.
        assert!(is_neural("kokoro/nobody"));
        assert!(voice("kokoro/nobody").is_none());
        assert_eq!(voice("supertonic/M2").map(|(_, v)| v.sid), Some(6));
        assert_eq!(model("piper-es").unwrap().size(), "67 MB");
        assert_eq!(model("kokoro").unwrap().size(), "350 MB");
    }
}
