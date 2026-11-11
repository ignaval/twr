//! The recordings `twr voice transcribe` takes: RIFF WAV, PCM, 16-bit, mono, 16 kHz (what
//! whisper.cpp reads, and what the clients record), between [`MIN_SECONDS`] and
//! [`MAX_SECONDS`] long.

use anyhow::{bail, Context};

/// The sample rate taken, in Hz.
pub const RATE: u32 = 16_000;
/// The longest recording taken, in seconds: one thing said, not a dictation.
pub const MAX_SECONDS: u32 = 120;
/// The shortest, in seconds: anything shorter is a click.
pub const MIN_SECONDS: f32 = 0.2;
/// The most bytes of a recording: [`MAX_SECONDS`] of samples and a header.
pub const MAX_BYTES: usize = (RATE as usize) * 2 * (MAX_SECONDS as usize) + 4096;

/// A recording checked by [`check`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recording {
    /// Its length, in seconds.
    pub seconds: f32,
    /// Its loudest sample, 0.0 to 1.0 (0: silence).
    pub peak: f32,
}

/// Checks that `bytes` is a recording twr takes; says why not.
pub fn check(bytes: &[u8]) -> anyhow::Result<Recording> {
    if bytes.len() > MAX_BYTES {
        bail!("the recording is longer than {MAX_SECONDS} seconds");
    }
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("the recording is not a WAV file");
    }
    let mut at = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = at + 8;
        let end = body.checked_add(len).context("a WAV chunk's length")?;
        match id {
            b"fmt " => {
                if len < 16 || end > bytes.len() {
                    bail!("the recording's format chunk is cut short");
                }
                let u16_at = |i: usize| u16::from_le_bytes([bytes[body + i], bytes[body + i + 1]]);
                let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
                format = Some((u16_at(0), u16_at(2), rate, u16_at(14)));
            }
            b"data" => {
                let Some((tag, channels, rate, bits)) = format else {
                    bail!("the recording has no format before its samples");
                };
                if tag != 1 || channels != 1 || rate != RATE || bits != 16 {
                    bail!(
                        "the recording must be 16 kHz mono 16-bit PCM (it is format {tag}, \
                         {channels} channel(s), {rate} Hz, {bits} bits)"
                    );
                }
                // A recorder that streams writes the length it knew then (0, or the most):
                // the samples are what the file holds.
                let data = &bytes[body..end.min(bytes.len())];
                let samples = data.len() / 2;
                let seconds = samples as f32 / RATE as f32;
                if seconds < MIN_SECONDS {
                    bail!("the recording is too short");
                }
                let peak = data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|s| i16::from_le_bytes(*s).unsigned_abs())
                    .max()
                    .unwrap_or(0);
                return Ok(Recording {
                    seconds,
                    peak: f32::from(peak) / 32768.0,
                });
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        at = end + (len & 1);
    }
    bail!("the recording has no samples")
}

/// A WAV file of `samples` (tests, and anyone who records raw samples).
pub fn encode(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds: f32, amplitude: i16) -> Vec<i16> {
        (0..(seconds * RATE as f32) as usize)
            .map(|i| if i % 20 < 10 { amplitude } else { -amplitude })
            .collect()
    }

    #[test]
    fn a_recording_is_measured() {
        let r = check(&encode(&tone(1.5, 16384))).unwrap();
        assert!((r.seconds - 1.5).abs() < 0.001, "{r:?}");
        assert!((r.peak - 0.5).abs() < 0.001, "{r:?}");
        assert_eq!(check(&encode(&vec![0; 8000])).unwrap().peak, 0.0);
    }

    #[test]
    fn only_16khz_mono_pcm_is_taken() {
        let good = encode(&tone(1.0, 1000));
        let mut stereo = good.clone();
        stereo[22] = 2;
        let mut rate = good.clone();
        rate[24..28].copy_from_slice(&44_100u32.to_le_bytes());
        let mut float = good.clone();
        float[20] = 3;
        for bad in [stereo, rate, float] {
            let e = check(&bad).unwrap_err().to_string();
            assert!(e.contains("16 kHz mono"), "{e}");
        }
        assert!(check(b"not a wav").is_err());
        assert!(check(&encode(&tone(0.1, 1000))).is_err(), "too short");
        assert!(check(&encode(&tone(121.0, 1000))).is_err(), "too long");
    }

    /// A streaming recorder's length field (0, or the most) is not trusted: the samples are
    /// what the file holds. Other chunks before the samples are skipped.
    #[test]
    fn the_samples_are_what_the_file_holds() {
        let mut w = encode(&tone(1.0, 1000));
        w[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!((check(&w).unwrap().seconds - 1.0).abs() < 0.001);
        let mut with_list = w[..36].to_vec();
        with_list.extend_from_slice(b"LIST");
        with_list.extend_from_slice(&3u32.to_le_bytes());
        with_list.extend_from_slice(b"abc\0");
        with_list.extend_from_slice(&w[36..]);
        assert!((check(&with_list).unwrap().seconds - 1.0).abs() < 0.001);
    }
}
