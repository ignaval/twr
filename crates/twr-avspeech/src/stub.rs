//! The shell of [`AvSpeech`] on a system without AVFAudio: it can't be made, so its methods
//! are never called.

use crate::{Error, Voice};

/// Never made: the type has no value.
enum Never {}

/// macOS's speech synthesizer: not on this system ([`AvSpeech::new`] fails).
pub struct AvSpeech {
    never: Never,
}

impl std::fmt::Debug for AvSpeech {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.never {}
    }
}

impl AvSpeech {
    /// Fails: "avspeech needs macOS".
    pub fn new() -> Result<AvSpeech, Error> {
        Err(Error::new("avspeech needs macOS"))
    }

    /// Never called (there is no [`AvSpeech`]).
    pub fn speak(
        &self,
        _text: &str,
        _voice: Option<&str>,
        _rate_wpm: Option<u32>,
        _volume: f32,
    ) -> Result<u64, Error> {
        match self.never {}
    }

    /// Never called.
    pub fn finished(&self, _id: u64) -> bool {
        match self.never {}
    }

    /// Never called.
    pub fn is_speaking(&self) -> bool {
        match self.never {}
    }

    /// Never called.
    pub fn is_paused(&self) -> bool {
        match self.never {}
    }

    /// Never called.
    pub fn pause(&self) -> bool {
        match self.never {}
    }

    /// Never called.
    pub fn resume(&self) -> bool {
        match self.never {}
    }

    /// Never called.
    pub fn stop(&self) {
        match self.never {}
    }

    /// Never called.
    pub fn voices(&self) -> Vec<Voice> {
        match self.never {}
    }
}
