//! Decoding for audio that arrives over the wire.
//!
//! The hotkey path hands `TranscriptionManager` 16 kHz mono f32 samples it
//! captured itself, so it never had to decode anything. A server accepts
//! whatever a client uploads, so this module is the missing adapter: WAV in
//! any common bit depth, sample rate and channel count out to the one shape
//! the engines accept.
//!
//! Deliberately WAV-only. Handy ships no audio decoder for compressed formats,
//! and pulling one in for the server would add a codec dependency to every
//! build. Clients get an explicit 415 instead of a silent mis-decode.

use anyhow::{anyhow, bail, Result};
use hound::{SampleFormat, WavReader};
use std::io::Cursor;
use std::time::Duration;

use crate::audio_toolkit::audio::FrameResampler;

/// Sample rate every transcription engine in Handy expects.
pub const TARGET_HZ: usize = 16_000;

/// Frame size the resampler emits in. Only affects internal chunking here —
/// the frames are concatenated before they reach the engine.
const RESAMPLE_FRAME: Duration = Duration::from_millis(20);

/// Longest upload accepted, in seconds of audio. A batch transcription holds
/// the whole decoded signal in memory and blocks the engine for its duration,
/// so an unbounded upload is a denial-of-service against the hotkey.
pub const MAX_AUDIO_SECS: f64 = 600.0;

/// Decode an uploaded WAV into 16 kHz mono f32 samples.
///
/// Returns the samples and the source sample rate (for logging/diagnostics).
pub fn decode_wav(bytes: &[u8]) -> Result<(Vec<f32>, u32)> {
    let reader = WavReader::new(Cursor::new(bytes))
        .map_err(|e| anyhow!("not a readable WAV file: {}", e))?;
    let spec = reader.spec();

    if spec.channels == 0 {
        bail!("WAV declares zero channels");
    }

    let interleaved = read_samples(reader, spec.sample_format, spec.bits_per_sample)?;
    let mono = downmix(&interleaved, spec.channels as usize);

    let secs = mono.len() as f64 / spec.sample_rate.max(1) as f64;
    if secs > MAX_AUDIO_SECS {
        bail!(
            "audio is {:.1}s, longer than the {:.0}s limit",
            secs,
            MAX_AUDIO_SECS
        );
    }

    let resampled = resample(mono, spec.sample_rate as usize);
    Ok((resampled, spec.sample_rate))
}

/// Decode raw little-endian PCM16 mono at `rate` — the wire format of the
/// streaming endpoint, where there is no container to describe the audio.
pub fn decode_pcm16(bytes: &[u8], rate: usize) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(2) {
        bail!("PCM16 payload has an odd byte count ({})", bytes.len());
    }
    let samples: Vec<f32> = bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32)
        .collect();
    Ok(resample(samples, rate))
}

fn read_samples(
    reader: WavReader<Cursor<&[u8]>>,
    format: SampleFormat,
    bits: u16,
) -> Result<Vec<f32>> {
    // hound decodes into the integer width that holds the sample, which is not
    // the declared bit depth: 24-bit packs into i32. Normalise by the declared
    // depth, not by the container width, or 24-bit comes out ~256x too quiet.
    let mut reader = reader;
    let samples = match (format, bits) {
        (SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()?,
        (SampleFormat::Int, 8) => reader
            .samples::<i8>()
            .map(|s| s.map(|v| v as f32 / i8::MAX as f32))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        (SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        (SampleFormat::Int, 24) => {
            let scale = 8_388_607.0_f32; // 2^23 - 1
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<std::result::Result<Vec<_>, _>>()?
        }
        (SampleFormat::Int, 32) => reader
            .samples::<i32>()
            .map(|s| s.map(|v| v as f32 / i32::MAX as f32))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        (f, b) => bail!("unsupported WAV sample format: {:?} at {} bits", f, b),
    };
    Ok(samples)
}

fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

fn resample(samples: Vec<f32>, from_hz: usize) -> Vec<f32> {
    if from_hz == TARGET_HZ || samples.is_empty() || from_hz == 0 {
        return samples;
    }
    let mut resampler = FrameResampler::new(from_hz, TARGET_HZ, RESAMPLE_FRAME);
    let mut out: Vec<f32> = Vec::with_capacity(samples.len() * TARGET_HZ / from_hz + 1024);
    resampler.push(&samples, |frame| out.extend_from_slice(frame));
    resampler.finish(|frame| out.extend_from_slice(frame));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{WavSpec, WavWriter};

    fn wav_i16(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let spec = WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = WavWriter::new(&mut buf, spec).unwrap();
            for s in samples {
                w.write_sample(*s).unwrap();
            }
            w.finalize().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn passes_16k_mono_through_untouched() {
        let bytes = wav_i16(16_000, 1, &[0, 16384, -16384, 0]);
        let (out, rate) = decode_wav(&bytes).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(out.len(), 4);
        assert!((out[1] - 0.5).abs() < 0.001, "got {}", out[1]);
    }

    #[test]
    fn downmixes_stereo_to_mono() {
        // Two frames of (L, R): (1.0, 0.0) and (0.0, 1.0) average to 0.5 each.
        let bytes = wav_i16(16_000, 2, &[i16::MAX, 0, 0, i16::MAX]);
        let (out, _) = decode_wav(&bytes).unwrap();
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < 0.01, "got {}", out[0]);
        assert!((out[1] - 0.5).abs() < 0.01, "got {}", out[1]);
    }

    #[test]
    fn resamples_44k_to_16k() {
        // One second of 44.1 kHz silence must come back as ~1s at 16 kHz.
        let bytes = wav_i16(44_100, 1, &vec![0i16; 44_100]);
        let (out, rate) = decode_wav(&bytes).unwrap();
        assert_eq!(rate, 44_100);
        let secs = out.len() as f64 / TARGET_HZ as f64;
        assert!(
            (secs - 1.0).abs() < 0.05,
            "got {:.3}s ({} samples)",
            secs,
            out.len()
        );
    }

    #[test]
    fn scales_24_bit_by_the_sample_width_not_the_container() {
        // i24 arrives in an i32 container, and dividing by i32::MAX instead of
        // 2^23-1 attenuates everything by 256 — audio that still decodes, still
        // transcribes, and is simply too quiet to hear. Nothing else in here
        // would fail if that constant were wrong.
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 24,
            sample_format: SampleFormat::Int,
        };
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = WavWriter::new(&mut buf, spec).unwrap();
            for s in [8_388_607i32, -8_388_607, 0] {
                w.write_sample(s).unwrap();
            }
            w.finalize().unwrap();
        }
        let (out, rate) = decode_wav(&buf.into_inner()).unwrap();
        assert_eq!(rate, 16_000);
        assert!((out[0] - 1.0).abs() < 0.001, "full scale decoded as {}", out[0]);
        assert!((out[1] + 1.0).abs() < 0.001, "full negative decoded as {}", out[1]);
    }

    #[test]
    fn rejects_non_wav() {
        assert!(decode_wav(b"ID3\x04\x00not actually a wav").is_err());
    }

    #[test]
    fn rejects_audio_over_the_length_limit() {
        // Declare a very low rate so a small buffer is a very long recording:
        // proves the limit is enforced on duration, not on byte count.
        let bytes = wav_i16(8_000, 1, &vec![0i16; 8_000 * 601]);
        let err = decode_wav(&bytes).unwrap_err().to_string();
        assert!(err.contains("longer than"), "got: {}", err);
    }

    #[test]
    fn decodes_pcm16_frames() {
        let raw: Vec<u8> = [0i16, i16::MAX, -i16::MAX]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let out = decode_pcm16(&raw, TARGET_HZ).unwrap();
        assert_eq!(out.len(), 3);
        assert!((out[1] - 1.0).abs() < 0.001);
    }

    #[test]
    fn rejects_odd_length_pcm16() {
        assert!(decode_pcm16(&[0u8, 1, 2], TARGET_HZ).is_err());
    }
}
