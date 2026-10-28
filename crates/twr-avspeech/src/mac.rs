//! [`AvSpeech`] on macOS: an AVSpeechSynthesizer on a thread of its own.
//!
//! Every Objective-C call is `unsafe` in objc2-avf-audio 0.3 (the bindings can't prove the
//! framework's own rules). Each one here is a plain message to an object made and kept on the
//! synthesizer's thread, with arguments of the types the bindings name, so the only rules to
//! keep are that the objects stay on that thread (they are never handed out) and that
//! autoreleased objects are released ([`autoreleasepool`] around each command).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use objc2::rc::{autoreleasepool, Retained};
use objc2_avf_audio::{
    AVSpeechBoundary, AVSpeechSynthesisVoice, AVSpeechSynthesizer, AVSpeechUtterance,
    AVSpeechUtteranceDefaultSpeechRate, AVSpeechUtteranceMaximumSpeechRate,
    AVSpeechUtteranceMinimumSpeechRate,
};
use objc2_foundation::NSString;

use crate::{find_voice, rate_for_wpm, Error, Voice, POLL};

/// How long a call waits for the synthesizer's thread to answer.
const ANSWER: Duration = Duration::from_secs(5);

/// How long an utterance may take to start speaking before it counts as said (the
/// synthesizer had nothing to say: an empty or unspeakable text).
const START_GRACE: Duration = Duration::from_secs(3);

/// A command for the synthesizer's thread.
enum Cmd {
    /// Say `text` as utterance `id`, after stopping what is being said.
    Speak {
        id: u64,
        text: String,
        voice: Option<String>,
        rate_wpm: Option<u32>,
        volume: f32,
        reply: mpsc::Sender<Result<(), Error>>,
    },
    /// Pause at the next word; whether it paused.
    Pause(mpsc::Sender<bool>),
    /// Go on; whether it did.
    Resume(mpsc::Sender<bool>),
    /// Stop at once.
    Stop(mpsc::Sender<()>),
    /// The system's voices.
    Voices(mpsc::Sender<Vec<Voice>>),
}

/// What the synthesizer's thread last saw, for calls that need no answer.
#[derive(Default)]
struct Status {
    /// It was speaking (or paused mid-utterance).
    speaking: AtomicBool,
    /// It was paused.
    paused: AtomicBool,
    /// Every utterance up to this id is over (said, stopped or replaced).
    finished: AtomicU64,
}

/// macOS's speech synthesizer, on a thread this value owns (see the crate documentation).
/// Dropping it stops the voice and ends the thread.
pub struct AvSpeech {
    tx: mpsc::Sender<Cmd>,
    status: Arc<Status>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for AvSpeech {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AvSpeech").finish_non_exhaustive()
    }
}

impl AvSpeech {
    /// Starts the synthesizer's thread.
    pub fn new() -> Result<AvSpeech, Error> {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Status::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let st = status.clone();
        std::thread::Builder::new()
            .name("twr-avspeech".into())
            .spawn(move || run(rx, st, ready_tx))
            .map_err(|e| Error::new(format!("starting the avspeech thread: {e}")))?;
        ready_rx
            .recv_timeout(ANSWER)
            .map_err(|_| Error::new("avspeech: the synthesizer did not start"))?;
        Ok(AvSpeech {
            tx,
            status,
            next_id: AtomicU64::new(1),
        })
    }

    /// Says `text` (stopping what is being said) with `voice` (an identifier or a name; `None`:
    /// the system's), at `rate_wpm` words per minute (`None`: the default), at `volume` (0 to
    /// 1). Returns at once with the utterance's id, for [`AvSpeech::finished`]. Fails when the
    /// voice is not there.
    pub fn speak(
        &self,
        text: &str,
        voice: Option<&str>,
        rate_wpm: Option<u32>,
        volume: f32,
    ) -> Result<u64, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = mpsc::channel();
        self.send(Cmd::Speak {
            id,
            text: text.to_string(),
            voice: voice.map(str::to_string),
            rate_wpm,
            volume: volume.clamp(0.0, 1.0),
            reply,
        })?;
        answer
            .recv_timeout(ANSWER)
            .map_err(|_| Error::new("avspeech did not answer"))??;
        Ok(id)
    }

    /// Whether utterance `id` is over: said, stopped, or replaced by a newer one.
    pub fn finished(&self, id: u64) -> bool {
        self.status.finished.load(Ordering::Acquire) >= id
    }

    /// Whether it is speaking (paused counts), as last seen (at most [`POLL`] ago).
    pub fn is_speaking(&self) -> bool {
        self.status.speaking.load(Ordering::Acquire)
    }

    /// Whether it is paused, as last seen.
    pub fn is_paused(&self) -> bool {
        self.status.paused.load(Ordering::Acquire)
    }

    /// Pauses at the next word boundary; whether it did (false with nothing to pause).
    pub fn pause(&self) -> bool {
        self.ask(Cmd::Pause).unwrap_or(false)
    }

    /// Goes on from where it paused; whether it did.
    pub fn resume(&self) -> bool {
        self.ask(Cmd::Resume).unwrap_or(false)
    }

    /// Stops at once (what was being said is over).
    pub fn stop(&self) {
        let _ = self.ask(Cmd::Stop);
    }

    /// The system's voices.
    pub fn voices(&self) -> Vec<Voice> {
        self.ask(Cmd::Voices).unwrap_or_default()
    }

    /// Sends `cmd` to the thread.
    fn send(&self, cmd: Cmd) -> Result<(), Error> {
        self.tx
            .send(cmd)
            .map_err(|_| Error::new("the avspeech thread ended"))
    }

    /// Sends the command `make` builds around a reply channel, and waits for the answer.
    fn ask<T>(&self, make: impl FnOnce(mpsc::Sender<T>) -> Cmd) -> Result<T, Error> {
        let (reply, answer) = mpsc::channel();
        self.send(make(reply))?;
        answer
            .recv_timeout(ANSWER)
            .map_err(|_| Error::new("avspeech did not answer"))
    }
}

/// The utterance being said: its id, when it was handed over, whether it was seen speaking.
struct Current {
    id: u64,
    at: Instant,
    started: bool,
}

/// The synthesizer's thread: commands as they come, and a look at the synthesizer every
/// [`POLL`]. Ends (stopping the voice) when the [`AvSpeech`] is dropped.
fn run(rx: mpsc::Receiver<Cmd>, status: Arc<Status>, ready: mpsc::Sender<()>) {
    // SAFETY: `new` makes a synthesizer, which stays on this thread.
    let synth: Retained<AVSpeechSynthesizer> = unsafe { AVSpeechSynthesizer::new() };
    let _ = ready.send(());
    let mut current: Option<Current> = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(cmd) => autoreleasepool(|_| handle(&synth, cmd, &status, &mut current)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // SAFETY: a message to the synthesizer, on its thread.
                unsafe { synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate) };
                return;
            }
        }
        // SAFETY: property reads of the synthesizer, on its thread.
        let (speaking, paused) = unsafe { (synth.isSpeaking(), synth.isPaused()) };
        status.speaking.store(speaking, Ordering::Release);
        status.paused.store(paused, Ordering::Release);
        if let Some(c) = &mut current {
            if speaking {
                c.started = true;
            } else if c.started || c.at.elapsed() > START_GRACE {
                status.finished.fetch_max(c.id, Ordering::AcqRel);
                current = None;
            }
        }
    }
}

/// Carries out `cmd` on the synthesizer.
fn handle(synth: &AVSpeechSynthesizer, cmd: Cmd, status: &Status, current: &mut Option<Current>) {
    match cmd {
        Cmd::Speak {
            id,
            text,
            voice,
            rate_wpm,
            volume,
            reply,
        } => {
            // What was being said is over: stopped now.
            // SAFETY: a message to the synthesizer, on its thread.
            unsafe { synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate) };
            if let Some(c) = current.take() {
                status.finished.fetch_max(c.id, Ordering::AcqRel);
            }
            match utterance(&text, voice.as_deref(), rate_wpm, volume) {
                Ok(u) => {
                    // SAFETY: a message to the synthesizer with an utterance made here.
                    unsafe { synth.speakUtterance(&u) };
                    *current = Some(Current {
                        id,
                        at: Instant::now(),
                        started: false,
                    });
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    status.finished.fetch_max(id, Ordering::AcqRel);
                    let _ = reply.send(Err(e));
                }
            }
        }
        Cmd::Pause(reply) => {
            // SAFETY: a message to the synthesizer, on its thread.
            let _ = reply.send(unsafe { synth.pauseSpeakingAtBoundary(AVSpeechBoundary::Word) });
        }
        Cmd::Resume(reply) => {
            // SAFETY: a message to the synthesizer, on its thread.
            let _ = reply.send(unsafe { synth.continueSpeaking() });
        }
        Cmd::Stop(reply) => {
            // SAFETY: a message to the synthesizer, on its thread.
            unsafe { synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate) };
            if let Some(c) = current.take() {
                status.finished.fetch_max(c.id, Ordering::AcqRel);
            }
            let _ = reply.send(());
        }
        Cmd::Voices(reply) => {
            let _ = reply.send(system_voices());
        }
    }
}

/// The utterance of `text` with `voice`, `rate_wpm` and `volume`.
fn utterance(
    text: &str,
    voice: Option<&str>,
    rate_wpm: Option<u32>,
    volume: f32,
) -> Result<Retained<AVSpeechUtterance>, Error> {
    let text = NSString::from_str(text);
    // SAFETY: a class method given a string made here.
    let u = unsafe { AVSpeechUtterance::speechUtteranceWithString(&text) };
    if let Some(wanted) = voice {
        let v = voice_named(wanted).ok_or_else(|| {
            Error::new(format!(
                "avspeech has no voice {wanted:?} (see System Settings, Accessibility, Spoken Content)"
            ))
        })?;
        // SAFETY: a setter given a voice the system returned.
        unsafe { u.setVoice(Some(&v)) };
    }
    if let Some(wpm) = rate_wpm {
        // SAFETY: reads of the framework's constant rates (plain floats).
        let (min, default, max) = unsafe {
            (
                AVSpeechUtteranceMinimumSpeechRate,
                AVSpeechUtteranceDefaultSpeechRate,
                AVSpeechUtteranceMaximumSpeechRate,
            )
        };
        // SAFETY: a setter given a float within the framework's limits.
        unsafe { u.setRate(rate_for_wpm(wpm, min, default, max)) };
    }
    // SAFETY: a setter given a float between 0 and 1.
    unsafe { u.setVolume(volume) };
    Ok(u)
}

/// The voice `wanted` names: its identifier, else its name among the system's voices.
fn voice_named(wanted: &str) -> Option<Retained<AVSpeechSynthesisVoice>> {
    let id = NSString::from_str(wanted);
    // SAFETY: a class method given a string made here.
    if let Some(v) = unsafe { AVSpeechSynthesisVoice::voiceWithIdentifier(&id) } {
        return Some(v);
    }
    let found = find_voice(&system_voices(), wanted)?.identifier.clone();
    let id = NSString::from_str(&found);
    // SAFETY: as above.
    unsafe { AVSpeechSynthesisVoice::voiceWithIdentifier(&id) }
}

/// The system's voices.
fn system_voices() -> Vec<Voice> {
    // SAFETY: a class method, then property reads of the voices it returned.
    unsafe {
        AVSpeechSynthesisVoice::speechVoices()
            .to_vec()
            .iter()
            .map(|v| Voice {
                identifier: v.identifier().to_string(),
                name: v.name().to_string(),
                language: v.language().to_string(),
            })
            .collect()
    }
}
