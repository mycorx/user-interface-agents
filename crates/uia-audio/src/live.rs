// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! State the playback and WASAPI backends keep across a device reopen: the
//! live device format and the mono playback queue.

use crate::level::PlaybackLevel;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

/// A device's sample rate and channel count, published as one word.
///
/// Two atomics would let a reader pair the new device's rate with the previous
/// one's channel count during a reopen. The window is microseconds and nothing
/// currently reads the channel count, but "safe because nobody looks" is the
/// kind of reasoning that stops being true quietly, and packing them costs a
/// shift.
// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
pub(crate) struct LiveFormat(AtomicU64);

// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
impl LiveFormat {
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub(crate) fn store(&self, sample_rate_hz: u32, channels: u16) {
        let packed = (u64::from(sample_rate_hz) << 16) | u64::from(channels);
        self.0.store(packed, Ordering::Relaxed);
    }

    pub(crate) fn load(&self) -> (u32, u16) {
        let packed = self.0.load(Ordering::Relaxed);
        ((packed >> 16) as u32, packed as u16)
    }
}

/// The mono samples waiting to be played, shared between the sink that fills
/// it and the device callback that drains it.
///
/// Held apart from the stream so a reopen onto another device can keep the
/// queue (and the level reading) while the stream underneath is replaced.
// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
#[derive(Clone, Default)]
pub(crate) struct PlaybackQueue {
    samples: Arc<Mutex<VecDeque<i16>>>,
    level: PlaybackLevel,
}

// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
impl PlaybackQueue {
    /// The queue, recovered if a panicking holder poisoned it: a `VecDeque`
    /// of samples has no invariant a half-finished `extend` can break, and a
    /// panic here would take the session (or the audio thread) down with it.
    fn samples(&self) -> MutexGuard<'_, VecDeque<i16>> {
        self.samples.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn push(&self, frame: &[i16]) {
        self.samples().extend(frame.iter().copied());
    }

    pub(crate) fn clear(&self) {
        self.samples().clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.samples().len()
    }

    pub(crate) fn level(&self) -> f32 {
        self.level.get()
    }

    /// The device callback body: fill `out`, interleaved over `channels`.
    ///
    /// Runs on the device's real-time thread, so it never blocks: when
    /// `push` or `clear` holds the queue (an `extend` can reallocate), this
    /// period plays silence and records level 0 rather than waiting on a
    /// tokio thread. A poisoned queue is recovered, never unwrapped -- a
    /// panic here would unwind into an `extern "C-unwind"` callback.
    ///
    /// The engine speaks mono, one queued sample per audio frame. A stereo
    /// (or wider) device interleaves N slots per frame, so each queued sample
    /// fans out to every slot in its frame -- popping one per *slot* would
    /// play two mono samples across L/R, halving playback duration and
    /// pitching the voice up like an unresampled rate mismatch does.
    pub(crate) fn try_fill(&self, out: &mut [f32], channels: usize) {
        let queue = match self.samples.try_lock() {
            Ok(queue) => Some(queue),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        match queue {
            Some(mut queue) => {
                for frame in out.chunks_mut(channels.max(1)) {
                    // Starved: pad with silence rather than block.
                    let sample = queue.pop_front().map_or(0.0, |s| f32::from(s) / 32768.0);
                    frame.fill(sample);
                }
            }
            None => out.fill(0.0),
        }
        self.level.record(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rate and channels must survive a round trip together, including the
    /// rates and channel counts real endpoints actually report.
    #[test]
    fn a_live_format_round_trips_rate_and_channels() {
        let live = LiveFormat::new();
        assert_eq!(live.load(), (0, 0), "starts unset, not garbage");
        for (rate, channels) in [(48_000u32, 2u16), (44_100, 1), (16_000, 8), (192_000, 2)] {
            live.store(rate, channels);
            assert_eq!(live.load(), (rate, channels));
        }
    }

    #[test]
    fn try_fill_fans_one_sample_across_every_channel_of_a_frame() {
        let queue = PlaybackQueue::default();
        queue.push(&[16_384, -16_384]);
        let mut out = [9.0f32; 6];
        queue.try_fill(&mut out, 3);
        assert_eq!(out, [0.5, 0.5, 0.5, -0.5, -0.5, -0.5]);
        assert_eq!(queue.len(), 0, "one sample consumed per frame");
        assert!(queue.level() > 0.0);
    }

    #[test]
    fn try_fill_pads_with_silence_when_starved_and_records_zero_level() {
        let queue = PlaybackQueue::default();
        queue.push(&[16_384]);
        let mut out = [0.0f32; 4];
        queue.try_fill(&mut out, 2);
        assert!(queue.level() > 0.0);
        queue.try_fill(&mut out, 2);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(queue.level(), 0.0);
    }

    /// `push` or `clear` holding the queue when the callback fires means
    /// silence for one period, never a blocked audio thread.
    #[test]
    fn a_contended_queue_plays_silence_and_records_zero_level() {
        let queue = PlaybackQueue::default();
        queue.push(&[16_384, 16_384]);
        let mut out = [0.0f32; 2];
        queue.try_fill(&mut out[..1], 1);
        assert!(queue.level() > 0.0);
        let held = queue.samples.lock().unwrap();
        let mut out = [9.0f32; 4];
        queue.try_fill(&mut out, 2);
        drop(held);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(queue.level(), 0.0);
        assert_eq!(queue.len(), 1, "a contended period consumes nothing");
    }

    /// A holder that panicked must not silence playback for good, nor panic
    /// the real-time thread.
    #[test]
    fn a_poisoned_queue_keeps_playing() {
        let queue = PlaybackQueue::default();
        queue.push(&[16_384]);
        let poisoner = queue.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.samples.lock().unwrap();
            panic!("poison the playback queue");
        })
        .join();
        assert!(queue.samples.is_poisoned());
        let mut out = [0.0f32; 1];
        queue.try_fill(&mut out, 1);
        assert_eq!(out, [0.5]);
        queue.push(&[-16_384]);
        assert_eq!(queue.len(), 1);
        queue.clear();
        assert_eq!(queue.len(), 0);
    }

    /// Barge-in: everything queued is dropped, so the next period is silence.
    #[test]
    fn clear_drops_every_queued_sample() {
        let queue = PlaybackQueue::default();
        queue.push(&[16_384, 16_384, 16_384]);
        queue.clear();
        assert_eq!(queue.len(), 0);
        let mut out = [9.0f32; 3];
        queue.try_fill(&mut out, 1);
        assert_eq!(out, [0.0; 3]);
        assert_eq!(queue.level(), 0.0);
    }
}
