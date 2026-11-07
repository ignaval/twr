//! twr's speech to text (`docs/voice.md`): the recording of one thing the user said into the
//! text of a message for the orchestrator. The device with the microphone records (the
//! desktop app, the mobile app's own recognizer); `twr voice transcribe` turns a recording
//! into text there, so no audio crosses to the hub.
//!
//! - [`wav`]: the only recordings taken: 16 kHz mono 16-bit PCM WAV, at most
//!   [`wav::MAX_SECONDS`].
//! - [`engine`]: what recognizes: whisper.cpp's CLI on this machine ([`engine::Kind::Whisper`])
//!   or a program of the user's ([`engine::Kind::Command`]), picked from `[voice]`
//!   ([`engine::resolve`]); [`engine::status`] says what is there for `twr voice status`.
//! - [`text`]: what a recognizer's output becomes: one line, without the markers whisper
//!   writes for silence or noise.
//!
//! The programs run without a shell, with fixed arguments: the recording is a private file
//! (whisper.cpp) or the program's stdin (`command`), never an argument a user's words reach.
//! Each runs in a process group of its own, killed whole on a timeout.

pub mod engine;
pub mod text;
pub mod wav;

pub use engine::{resolve, status, transcribe, Kind, Status};
