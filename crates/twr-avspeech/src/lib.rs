//! twr's `avspeech` voice (`[speech] engine = "avspeech"`): macOS's AVSpeechSynthesizer
//! (AVFAudio), driven in-process through objc2's bindings.
//!
//! - **One thread.** The synthesizer and every Objective-C object around it stay on a thread
//!   of their own, which [`AvSpeech`] starts and owns: they are not `Send`. Calls reach it over
//!   a channel and wait (briefly) for its answer.
//! - **No delegate, no run loop.** The thread polls `isSpeaking` every [`POLL`] between
//!   commands and records what it saw ([`AvSpeech::finished`], [`AvSpeech::is_speaking`]), so
//!   nothing needs a delegate object or a run loop.
//! - **Controls.** [`AvSpeech::pause`] pauses at the next word, [`AvSpeech::resume`] goes on
//!   from there, [`AvSpeech::stop`] stops at once. A new [`AvSpeech::speak`] stops what was
//!   being said first.
//! - **Elsewhere** (not macOS) the crate is an empty shell: [`AvSpeech::new`] fails with
//!   "avspeech needs macOS".
//!
//! Not yet run on a real Mac: checked by `cargo check --target aarch64-apple-darwin` only, so
//! twr's default macOS engine is `say` and this one is opt-in.

use std::fmt;
use std::time::Duration;

/// How often the synthesizer's thread looks at whether it is still speaking.
pub const POLL: Duration = Duration::from_millis(50);

/// The words per minute AVSpeechSynthesizer's default rate (`0.5`) is taken to be.
pub const DEFAULT_WPM: u32 = 180;

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    /// An error saying `what`.
    pub fn new(what: impl Into<String>) -> Error {
        Error(what.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// A voice the system has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice {
    /// Its identifier (`com.apple.voice.compact.en-US.Samantha`).
    pub identifier: String,
    /// Its name (`Samantha`).
    pub name: String,
    /// Its language (BCP-47, `en-US`).
    pub language: String,
}

/// The utterance rate for `wpm` words per minute: AVSpeechSynthesizer's rate is not in words
/// per minute, so this scales its `default` (taken as [`DEFAULT_WPM`]) by `wpm`, kept between
/// `min` and `max` (`AVSpeechUtteranceMinimumSpeechRate` and `...Maximum...`). An
/// approximation: the synthesizer's rate is not linear in words per minute.
pub fn rate_for_wpm(wpm: u32, min: f32, default: f32, max: f32) -> f32 {
    (default * wpm as f32 / DEFAULT_WPM as f32).clamp(min, max)
}

/// The voice of `voices` that `wanted` names: by identifier, else by name (ignoring case).
pub fn find_voice<'a>(voices: &'a [Voice], wanted: &str) -> Option<&'a Voice> {
    voices
        .iter()
        .find(|v| v.identifier == wanted)
        .or_else(|| voices.iter().find(|v| v.name.eq_ignore_ascii_case(wanted)))
}

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::AvSpeech;

#[cfg(not(target_os = "macos"))]
mod stub;
#[cfg(not(target_os = "macos"))]
pub use stub::AvSpeech;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpm_scales_the_default_rate_within_the_limits() {
        assert_eq!(rate_for_wpm(180, 0.0, 0.5, 1.0), 0.5);
        assert_eq!(rate_for_wpm(90, 0.0, 0.5, 1.0), 0.25);
        assert_eq!(rate_for_wpm(500, 0.0, 0.5, 1.0), 1.0);
        assert_eq!(rate_for_wpm(10, 0.1, 0.5, 1.0), 0.1);
    }

    #[test]
    fn a_voice_is_found_by_identifier_then_by_name() {
        let voices = vec![
            Voice {
                identifier: "com.apple.voice.compact.en-US.Samantha".into(),
                name: "Samantha".into(),
                language: "en-US".into(),
            },
            Voice {
                identifier: "Ava".into(),
                name: "Not Ava".into(),
                language: "en-US".into(),
            },
        ];
        assert_eq!(find_voice(&voices, "samantha").unwrap().name, "Samantha");
        assert_eq!(find_voice(&voices, "Ava").unwrap().name, "Not Ava");
        assert!(find_voice(&voices, "Zarvox").is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn elsewhere_it_needs_macos() {
        let e = AvSpeech::new().unwrap_err();
        assert_eq!(e.to_string(), "avspeech needs macOS");
    }
}
