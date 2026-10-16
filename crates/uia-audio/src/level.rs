// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! What a sink's `AudioSink::level` reports: the RMS of the period the
//! device callback most recently played.
//!
//! Recorded on the realtime thread, where the queue is drained, because that
//! is the only place that knows what is audible. `write` sees engine audio
//! arriving faster than real time, so a level taken there would run ahead of
//! the voice and go flat while the tail of a reply is still playing. An
//! atomic rather than a lock so the callback can never block on a reader.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone, Default)]
pub(crate) struct PlaybackLevel(Arc<AtomicU32>);

impl PlaybackLevel {
    /// Record one played period. Interleaved multichannel is fine: every
    /// backend fans one mono sample across the frame, which leaves the RMS
    /// unchanged. Silence (an underrun, a cleared queue) records 0.0.
    pub(crate) fn record(&self, played: &[f32]) {
        let rms = if played.is_empty() {
            0.0
        } else {
            let sum: f32 = played.iter().map(|s| s * s).sum();
            (sum / played.len() as f32).sqrt().min(1.0)
        };
        self.0.store(rms.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::PlaybackLevel;

    #[test]
    fn starts_silent() {
        assert_eq!(PlaybackLevel::default().get(), 0.0);
    }

    #[test]
    fn a_half_scale_period_is_rms_half() {
        let level = PlaybackLevel::default();
        level.record(&[0.5, -0.5, 0.5, -0.5]);
        assert!((level.get() - 0.5).abs() < 1e-6, "got {}", level.get());
    }

    #[test]
    fn a_silent_period_after_speech_drops_to_zero() {
        // An underrun or a barge-in `clear()` must not leave the wave standing.
        let level = PlaybackLevel::default();
        level.record(&[0.5; 8]);
        level.record(&[0.0; 8]);
        assert_eq!(level.get(), 0.0);
    }

    #[test]
    fn clones_share_one_reading() {
        // The callback holds one clone and the sink the other.
        let sink_side = PlaybackLevel::default();
        let callback_side = sink_side.clone();
        callback_side.record(&[1.0; 4]);
        assert_eq!(sink_side.get(), 1.0);
    }
}
