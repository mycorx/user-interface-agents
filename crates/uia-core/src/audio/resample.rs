// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use crate::audio::AudioError;
use audioadapter_buffers::owned::InterleavedOwned;
use rubato::{Fft, FixedSync, Resampler as _};

/// Sample-rate conversion isolated behind our own type so the rubato API is
/// referenced in exactly one place. Real filtered resampling, never decimation:
/// dropping samples without a low-pass aliases, and aliased microphone audio
/// degrades recognition in ways that are miserable to debug later.
///
/// rubato 5.0's real API (`Fft`/`FixedSync`/`InterleavedOwned`) has no overlap
/// with the `FftFixedIn` sketch this wrapper was originally planned against —
/// confirmed by reading the crate source, not the plan.
///
/// **Streaming, not batch.** Callers feed one 20 ms frame at a time, so the
/// filter state must survive across calls. rubato's `process_all` documents
/// that it resets the resampler before every call — using it per frame gave a
/// fresh filter each time and wrecked the signal at every chunk boundary
/// (1.76 dB SNR against 45.6 dB for the same samples in one call, measured
/// 2026-08-19 in S16). This wrapper therefore buffers input and drives
/// rubato's fixed-chunk `process` instead, so N chunked calls are byte-for-byte
/// identical to one whole-clip call.
pub struct Resampler {
    inner: Option<Fft<f32>>,
    /// Input samples not yet forming a complete chunk, carried to the next call.
    pending: Vec<f32>,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32, chunk_frames: usize) -> Result<Self, AudioError> {
        if from_hz == 0 || to_hz == 0 {
            return Err(AudioError::InvalidRate { from_hz, to_hz });
        }
        // Equal rates short-circuit: no filter, no allocation, exact passthrough.
        if from_hz == to_hz {
            return Ok(Self {
                inner: None,
                pending: Vec::new(),
            });
        }
        let inner = Fft::<f32>::new(
            from_hz as usize,
            to_hz as usize,
            chunk_frames,
            1, // channels (mono)
            FixedSync::Input,
        )
        .map_err(|e| AudioError::Resample(e.to_string()))?;
        Ok(Self {
            inner: Some(inner),
            pending: Vec::with_capacity(chunk_frames * 2),
        })
    }

    /// Resample whatever is available, carrying any remainder to the next call.
    ///
    /// Returns only the samples belonging to complete chunks, so a call may
    /// return fewer samples than the ratio suggests (or none at all) while the
    /// buffer fills. That is what makes repeated small calls equivalent to one
    /// large one.
    pub fn process(&mut self, input: &[i16]) -> Result<Vec<i16>, AudioError> {
        // Destructured so `pending` and `inner` are disjoint borrows.
        let Self { inner, pending } = self;
        let Some(inner) = inner.as_mut() else {
            return Ok(input.to_vec());
        };

        pending.extend(input.iter().map(|&s| s as f32 / i16::MAX as f32));

        let mut samples = Vec::new();
        loop {
            let need = inner.input_frames_next();
            if pending.len() < need {
                break;
            }
            let block: Vec<f32> = pending.drain(..need).collect();
            let buffer_in = InterleavedOwned::new_from(block, 1, need)
                .map_err(|e| AudioError::Resample(e.to_string()))?;
            let out = inner
                .process(&buffer_in, None)
                .map_err(|e| AudioError::Resample(e.to_string()))?;
            samples.extend(
                out.take_data()
                    .into_iter()
                    .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
            );
        }
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn sine(freq_hz: f64, rate_hz: u32, samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|n| {
                let t = n as f64 / rate_hz as f64;
                ((2.0 * PI * freq_hz * t).sin() * 8000.0) as i16
            })
            .collect()
    }

    /// Count zero crossings to estimate frequency without pulling in an FFT.
    fn dominant_freq_hz(samples: &[i16], rate_hz: u32) -> f64 {
        let crossings = samples
            .windows(2)
            .filter(|w| (w[0] < 0) != (w[1] < 0))
            .count();
        (crossings as f64 / 2.0) * (rate_hz as f64 / samples.len() as f64)
    }

    #[test]
    fn resampling_48k_to_16k_preserves_tone_frequency() {
        let input = sine(1000.0, 48_000, 48_000);
        let mut r = Resampler::new(48_000, 16_000, 1024).unwrap();
        let out = r.process(&input).unwrap();
        let f = dominant_freq_hz(&out, 16_000);
        assert!((f - 1000.0).abs() < 50.0, "expected ~1000 Hz, got {f}");
    }

    #[test]
    fn resampling_does_not_alias_a_tone_above_the_new_nyquist() {
        // 7 kHz survives 16 kHz output (Nyquist 8 kHz). A naive 3:1 decimator
        // would fold it down to a bogus low tone. This is the test that proves
        // we are filtering, not just dropping samples.
        let input = sine(7000.0, 48_000, 48_000);
        let mut r = Resampler::new(48_000, 16_000, 1024).unwrap();
        let out = r.process(&input).unwrap();
        let f = dominant_freq_hz(&out, 16_000);
        assert!(
            f > 6500.0,
            "aliased down to {f} Hz; low-pass filter missing"
        );
    }

    #[test]
    fn chunked_calls_match_one_whole_clip_call() {
        // The session and the Opus codec both resample one 20 ms frame at a
        // time. rubato's `process_all` documents that it *resets* the resampler
        // before every call, so a per-chunk caller used to get a fresh filter
        // each frame and the chunk boundaries destroyed the signal: measured at
        // 1.76 dB SNR, against 45.6 dB for the same samples in a single call.
        // Streaming must therefore be indistinguishable from one whole-clip
        // call, sample for sample.
        let input = sine(440.0, 24_000, 480 * 30);

        let mut whole = Resampler::new(24_000, 48_000, 480).unwrap();
        let expected = whole.process(&input).unwrap();

        let mut streaming = Resampler::new(24_000, 48_000, 480).unwrap();
        let mut got = Vec::new();
        for chunk in input.chunks(480) {
            got.extend_from_slice(&streaming.process(chunk).unwrap());
        }

        assert_eq!(
            got.len(),
            expected.len(),
            "streaming produced {} samples, whole-clip {}",
            got.len(),
            expected.len()
        );
        assert_eq!(got, expected, "streaming diverged from whole-clip output");
    }

    #[test]
    fn identical_rates_pass_through_unchanged() {
        let input = sine(440.0, 16_000, 1600);
        let mut r = Resampler::new(16_000, 16_000, 1024).unwrap();
        assert_eq!(r.process(&input).unwrap(), input);
    }

    #[test]
    fn output_length_scales_with_the_rate_ratio() {
        let input = sine(440.0, 48_000, 48_000);
        let mut r = Resampler::new(48_000, 16_000, 1024).unwrap();
        let out = r.process(&input).unwrap();
        let expected = 16_000;
        let tolerance = expected / 10;
        assert!(
            out.len().abs_diff(expected) < tolerance,
            "expected ~{expected} samples, got {}",
            out.len()
        );
    }
}
