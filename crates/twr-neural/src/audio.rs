//! Playing twr's voices: one output stream on the system's default device (cpal: CoreAudio
//! on macOS, ALSA on Linux, which PipeWire and PulseAudio serve), fed a sentence at a time,
//! so sentences follow each other with no gap (a player program per sentence leaves most of
//! a second between them: `afplay` starts and stops the device each time).
//!
//! - **One thread** owns the stream (it is not `Send`): it opens it when there is something
//!   to play and closes it once nothing played for [`CLOSE_AFTER`], so an idle speaker holds
//!   no audio device.
//! - **Positions.** [`Output::queue`] returns where its clip ends, counted in the device's
//!   frames since the output was made; [`Output::played`] how far the device got. A chunk is
//!   said when the device passed its last clip's end.
//! - **Stop and pause.** [`Output::clear`] drops what is queued at once; [`Output::pause`]
//!   plays silence without moving on, so resuming goes on mid-word.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::synth::Clip;

/// How long the stream stays open with nothing to play.
pub const CLOSE_AFTER: Duration = Duration::from_secs(3);

/// What the stream's callback and the rest share.
#[derive(Default)]
struct Shared {
    /// Samples at the device's rate, mono, not played yet.
    queue: Mutex<VecDeque<f32>>,
    /// Frames the device took from the queue (silence while paused or empty doesn't count).
    played: AtomicU64,
    /// Frames ever queued (cleared ones count as played).
    queued: AtomicU64,
    paused: AtomicBool,
    /// The device's rate, once the stream was first opened.
    rate: AtomicU64,
    /// The last error of the stream (no device, the device went away).
    error: Mutex<Option<String>>,
    /// The stream failed: the thread opens a new one for what comes next.
    broken: AtomicBool,
}

/// The output (see the module documentation). Cheap to clone.
#[derive(Clone)]
pub struct Output {
    shared: Arc<Shared>,
    wake: mpsc::Sender<()>,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Output")
    }
}

impl Default for Output {
    fn default() -> Self {
        Output::new()
    }
}

impl Output {
    /// An output on the default device, opened when first played to.
    pub fn new() -> Output {
        let shared = Arc::new(Shared::default());
        let (wake, rx) = mpsc::channel::<()>();
        let s = shared.clone();
        std::thread::Builder::new()
            .name("twr-audio".into())
            .spawn(move || run(s, rx))
            .expect("starting the twr-audio thread");
        Output { shared, wake }
    }

    /// The device's rate, opening the stream to learn it if it never was.
    fn rate(&self) -> anyhow::Result<u32> {
        let r = self.shared.rate.load(Ordering::SeqCst);
        if r > 0 {
            return Ok(r as u32);
        }
        let r = device_rate()?;
        self.shared.rate.store(r as u64, Ordering::SeqCst);
        Ok(r)
    }

    /// Queues `clip` at `volume`; where it ends ([`Output::played`] reaches it).
    pub fn queue(&self, clip: &Clip, volume: f32) -> anyhow::Result<u64> {
        let rate = self.rate()?;
        let samples = resample(&clip.samples, clip.sample_rate, rate);
        let volume = volume.clamp(0.0, 1.0);
        let n = samples.len() as u64;
        {
            let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            q.extend(samples.into_iter().map(|s| (s * volume).clamp(-1.0, 1.0)));
        }
        let end = self.shared.queued.fetch_add(n, Ordering::SeqCst) + n;
        let _ = self.wake.send(());
        Ok(end)
    }

    /// Frames played so far.
    pub fn played(&self) -> u64 {
        self.shared.played.load(Ordering::SeqCst)
    }

    /// The stream's last error, taken.
    pub fn take_error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// Drops what is queued: silence at once.
    pub fn clear(&self) {
        let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        let dropped = q.len() as u64;
        q.clear();
        self.shared.played.fetch_add(dropped, Ordering::SeqCst);
    }

    /// Pauses (silence, nothing moves on) or goes on.
    pub fn pause(&self, on: bool) {
        self.shared.paused.store(on, Ordering::SeqCst);
        let _ = self.wake.send(());
    }

    /// Waits until the device played up to `end` (or fails with the stream's error).
    pub async fn until(&self, end: u64) -> anyhow::Result<()> {
        while self.played() < end {
            if let Some(e) = self.take_error() {
                self.clear();
                return Err(anyhow!("audio output: {e}"));
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        Ok(())
    }
}

/// Whether this machine has a default output device.
pub fn has_output() -> bool {
    cpal::default_host().default_output_device().is_some()
}

fn device_rate() -> anyhow::Result<u32> {
    let device = cpal::default_host()
        .default_output_device()
        .context("no audio output device")?;
    let config = device
        .default_output_config()
        .context("the audio output device's format")?;
    Ok(config.sample_rate())
}

/// The stream's thread: opens it when there is something to play, closes it when idle.
fn run(shared: Arc<Shared>, wake: mpsc::Receiver<()>) {
    let mut stream: Option<cpal::Stream> = None;
    let mut idle_since: Option<Instant> = None;
    while !matches!(
        wake.recv_timeout(Duration::from_millis(250)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ) {
        if shared.broken.swap(false, Ordering::SeqCst) {
            stream = None;
        }
        let pending = !shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty();
        if pending {
            idle_since = None;
            if stream.is_none() {
                match open(&shared) {
                    Ok(s) => stream = Some(s),
                    Err(e) => {
                        *shared.error.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some(format!("{e:#}"));
                    }
                }
            }
        } else if stream.is_some() {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= CLOSE_AFTER && !shared.paused.load(Ordering::SeqCst) {
                stream = None;
                idle_since = None;
            }
        }
    }
}

/// Opens the default device's stream, its callback taking from the queue.
fn open(shared: &Arc<Shared>) -> anyhow::Result<cpal::Stream> {
    let device = cpal::default_host()
        .default_output_device()
        .context("no audio output device")?;
    let supported = device
        .default_output_config()
        .context("the audio output device's format")?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    shared
        .rate
        .store(config.sample_rate as u64, Ordering::SeqCst);
    let channels = config.channels as usize;
    let err_shared = shared.clone();
    let on_error = move |e: cpal::Error| {
        *err_shared.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
        err_shared.broken.store(true, Ordering::SeqCst);
    };
    macro_rules! build {
        ($t:ty, $conv:expr) => {{
            let s = shared.clone();
            device.build_output_stream(
                config.clone(),
                move |out: &mut [$t], _| {
                    fill(&s, out, channels, $conv);
                },
                on_error,
                None,
            )?
        }};
    }
    let stream = match format {
        cpal::SampleFormat::F32 => build!(f32, |v: f32| v),
        cpal::SampleFormat::I16 => build!(i16, |v: f32| (v * i16::MAX as f32) as i16),
        cpal::SampleFormat::U16 => build!(u16, |v: f32| ((v + 1.0) * 0.5 * u16::MAX as f32) as u16),
        cpal::SampleFormat::I32 => build!(i32, |v: f32| (v as f64 * i32::MAX as f64) as i32),
        other => anyhow::bail!("the audio output device's format {other:?} is not one twr plays"),
    };
    stream.play().context("starting the audio output")?;
    Ok(stream)
}

/// The callback: each frame the next queued sample on every channel, silence when paused or
/// out of samples.
fn fill<T: Copy>(shared: &Shared, out: &mut [T], channels: usize, conv: impl Fn(f32) -> T) {
    let paused = shared.paused.load(Ordering::Relaxed);
    let mut q = match shared.queue.try_lock() {
        Ok(q) => q,
        // Being queued to this instant: silence for one buffer rather than waiting.
        Err(_) => {
            out.fill(conv(0.0));
            return;
        }
    };
    let mut taken = 0u64;
    for frame in out.chunks_mut(channels.max(1)) {
        let v = if paused {
            0.0
        } else {
            match q.pop_front() {
                Some(v) => {
                    taken += 1;
                    v
                }
                None => 0.0,
            }
        };
        frame.fill(conv(v));
    }
    drop(q);
    shared.played.fetch_add(taken, Ordering::SeqCst);
}

/// `samples` at `from` Hz as `to` Hz (linear: speech has little above 8 kHz).
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let n = (samples.len() as u64 * to as u64 / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..n)
        .map(|i| {
            let x = i as f64 * step;
            let j = x as usize;
            let frac = (x - j as f64) as f32;
            let a = samples[j.min(samples.len() - 1)];
            let b = samples[(j + 1).min(samples.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_length_and_shape() {
        let s: Vec<f32> = (0..24_000).map(|i| i as f32 / 24_000.0).collect();
        let r = resample(&s, 24_000, 48_000);
        assert_eq!(r.len(), 48_000);
        assert!((r[24_000] - 0.5).abs() < 1e-3);
        assert_eq!(resample(&s, 24_000, 24_000).len(), 24_000);
        assert_eq!(resample(&s, 44_100, 22_050).len(), 12_000);
        assert!(resample(&[], 24_000, 48_000).is_empty());
    }

    #[test]
    fn the_callback_plays_the_queue_on_every_channel() {
        let shared = Shared::default();
        shared.queue.lock().unwrap().extend([0.5, -0.5]);
        let mut out = [9.0f32; 6];
        fill(&shared, &mut out, 2, |v| v);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5, 0.0, 0.0]);
        assert_eq!(shared.played.load(Ordering::SeqCst), 2);
        // Paused: silence, nothing taken.
        shared.queue.lock().unwrap().push_back(0.25);
        shared.paused.store(true, Ordering::SeqCst);
        fill(&shared, &mut out, 2, |v| v);
        assert_eq!(out, [0.0; 6]);
        assert_eq!(shared.played.load(Ordering::SeqCst), 2);
    }
}
