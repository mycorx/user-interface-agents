// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use crate::audio::AudioFormat;
use async_trait::async_trait;
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("audio device unavailable: {0}")]
    DeviceUnavailable(String),
    #[error("resampling failed: {0}")]
    Resample(String),
    #[error("invalid sample rate: {from_hz} -> {to_hz}")]
    InvalidRate { from_hz: u32, to_hz: u32 },
}

/// What a direction is waiting for while its device is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitingFor {
    /// A device the user pinned by name, which is never replaced by another.
    Named(String),
    /// Whatever the OS default is, once there is one.
    Default,
}

/// Where a backend says what it is waiting for, read by the session.
///
/// A shared cell rather than a channel: the session polls it once per pass of
/// its run loop and only ever wants the current answer. Cloning shares the
/// one value, so the backend keeps a clone and hands the session another.
#[derive(Clone, Default)]
pub struct WaitingCell(Arc<Mutex<Option<WaitingFor>>>);

impl WaitingCell {
    /// `None` once the device is open again.
    pub fn set(&self, waiting: Option<WaitingFor>) {
        *self.lock() = waiting;
    }

    pub fn get(&self) -> Option<WaitingFor> {
        self.lock().clone()
    }

    /// A panic elsewhere while holding the lock leaves a plain value behind,
    /// still valid to read and overwrite, so poisoning is not worth failing
    /// an audio thread over.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<WaitingFor>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
pub trait AudioSource: Send {
    /// The format of the frames this source yields. Read right after
    /// `next_frame` returns, it describes *that* frame: the session tags each
    /// frame with the rate read there, so a source whose device reopens at a
    /// new rate must report the new rate from the first frame it yields at
    /// that rate, and not before.
    fn format(&self) -> AudioFormat;
    /// Yields the next captured frame, or `None` when the source is exhausted.
    ///
    /// Must be cancel-safe: the session races this against a stop signal and
    /// drops the future when the stop wins, so dropping it before it resolves
    /// must lose no frame that a later call would otherwise have yielded.
    async fn next_frame(&mut self) -> Option<Vec<i16>>;
    /// Where this source reports waiting for its device, if it ever does.
    /// `None` for a source that is either open or gone, never waiting.
    fn waiting(&self) -> Option<WaitingCell> {
        None
    }
}

#[async_trait]
pub trait AudioSink: Send {
    fn format(&self) -> AudioFormat;
    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError>;
    /// Drop all buffered audio immediately. Synchronous and non-blocking by
    /// contract: barge-in must not await anything, least of all the network.
    fn clear(&mut self);
    /// RMS (0.0..=1.0) of the audio the device most recently played — what
    /// the HUD's wave follows, so it moves with the assistant's voice and
    /// never with the user's. Measured where the queue is drained rather than
    /// where `write` fills it: engines stream faster than real time, so a
    /// level taken on `write` would run ahead of what is audible and fall flat
    /// while the tail of a reply is still playing. Starved or cleared is 0.0.
    fn level(&self) -> f32 {
        0.0
    }
    /// The output-side twin of `AudioSource::waiting`.
    fn waiting(&self) -> Option<WaitingCell> {
        None
    }
}

pub struct FixtureSource {
    format: AudioFormat,
    frames: std::collections::VecDeque<Vec<i16>>,
}

impl FixtureSource {
    pub fn new(format: AudioFormat, frames: Vec<Vec<i16>>) -> Self {
        Self {
            format,
            frames: frames.into(),
        }
    }
}

#[async_trait]
impl AudioSource for FixtureSource {
    fn format(&self) -> AudioFormat {
        self.format
    }
    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        self.frames.pop_front()
    }
}

/// A source with no device behind it: `next_frame` never resolves.
///
/// What a session holds while it has no microphone — before one is opened,
/// or between closing one device and opening the next. Never resolving,
/// rather than yielding `None`, is the point: `None` means the source is
/// exhausted and ends the run loop, while a missing microphone is a state
/// the session waits out.
pub struct DetachedSource {
    format: AudioFormat,
}

impl DetachedSource {
    pub fn new(format: AudioFormat) -> Self {
        Self { format }
    }
}

#[async_trait]
impl AudioSource for DetachedSource {
    fn format(&self) -> AudioFormat {
        self.format
    }
    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        std::future::pending().await
    }
}

/// A sink with no device behind it: writes are discarded and nothing ever
/// plays, so its level is always 0.0. The output-side twin of
/// `DetachedSource`.
pub struct DetachedSink {
    format: AudioFormat,
}

impl DetachedSink {
    pub fn new(format: AudioFormat) -> Self {
        Self { format }
    }
}

#[async_trait]
impl AudioSink for DetachedSink {
    fn format(&self) -> AudioFormat {
        self.format
    }
    async fn write(&mut self, _frame: &[i16]) -> Result<(), AudioError> {
        Ok(())
    }
    fn clear(&mut self) {}
}

/// A handle onto a `VecSink`'s observations that survives the sink being moved
/// into a `Session`. Cloning shares the same underlying buffer, so a test can
/// keep watching a sink it no longer owns.
#[derive(Clone, Default)]
pub struct SinkProbe {
    buf: std::sync::Arc<std::sync::Mutex<Vec<i16>>>,
    clears: std::sync::Arc<std::sync::Mutex<usize>>,
    /// RMS of the last write. A `VecSink` has no playback clock, so it treats
    /// each write as played the moment it lands.
    level: std::sync::Arc<std::sync::Mutex<f32>>,
}

impl SinkProbe {
    pub fn written(&self) -> Vec<i16> {
        self.buf.lock().unwrap().clone()
    }
    pub fn clear_count(&self) -> usize {
        *self.clears.lock().unwrap()
    }
}

#[derive(Default)]
pub struct VecSink {
    format: Option<AudioFormat>,
    probe: SinkProbe,
}

impl VecSink {
    pub fn new(format: AudioFormat) -> Self {
        Self {
            format: Some(format),
            probe: SinkProbe::default(),
        }
    }
    /// Take an observation handle before moving this sink into a `Session`.
    pub fn probe(&self) -> SinkProbe {
        self.probe.clone()
    }
    pub fn written(&self) -> Vec<i16> {
        self.probe.written()
    }
    pub fn clear_count(&self) -> usize {
        self.probe.clear_count()
    }
}

#[async_trait]
impl AudioSink for VecSink {
    fn format(&self) -> AudioFormat {
        self.format.unwrap_or(AudioFormat::mono_pcm16(24_000))
    }
    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError> {
        self.probe.buf.lock().unwrap().extend_from_slice(frame);
        *self.probe.level.lock().unwrap() = crate::audio::rms(frame);
        Ok(())
    }
    fn clear(&mut self) {
        self.probe.buf.lock().unwrap().clear();
        *self.probe.clears.lock().unwrap() += 1;
        *self.probe.level.lock().unwrap() = 0.0;
    }
    fn level(&self) -> f32 {
        *self.probe.level.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixture_source_yields_frames_then_ends() {
        let mut src = FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![vec![1, 2], vec![3, 4]],
        );
        assert_eq!(src.next_frame().await, Some(vec![1, 2]));
        assert_eq!(src.next_frame().await, Some(vec![3, 4]));
        assert_eq!(src.next_frame().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_detached_source_never_yields_a_frame_and_never_ends() {
        let mut src = DetachedSource::new(AudioFormat::mono_pcm16(48_000));
        let waited =
            tokio::time::timeout(std::time::Duration::from_secs(60), src.next_frame()).await;
        assert!(waited.is_err(), "a detached source yielded {waited:?}");
        assert_eq!(src.format(), AudioFormat::mono_pcm16(48_000));
    }

    #[tokio::test]
    async fn a_detached_sink_accepts_writes_and_plays_nothing() {
        let mut sink = DetachedSink::new(AudioFormat::mono_pcm16(24_000));
        sink.write(&[i16::MAX; 480]).await.unwrap();
        sink.clear();
        assert_eq!(sink.level(), 0.0);
        assert_eq!(sink.format(), AudioFormat::mono_pcm16(24_000));
    }

    #[test]
    fn waiting_cell_clones_share_one_value() {
        // The backend keeps one clone and the session reads another: they
        // must be one value, or the HUD never hears what the device is doing.
        let backend = WaitingCell::default();
        let session = backend.clone();
        assert_eq!(session.get(), None);

        backend.set(Some(WaitingFor::Named("Jabra".into())));
        assert_eq!(session.get(), Some(WaitingFor::Named("Jabra".into())));

        session.set(None);
        assert_eq!(backend.get(), None);
    }

    #[tokio::test]
    async fn vec_sink_accumulates_written_frames() {
        let mut sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
        sink.write(&[1, 2]).await.unwrap();
        sink.write(&[3]).await.unwrap();
        assert_eq!(sink.written(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn clear_discards_buffered_audio_and_is_observable() {
        let mut sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
        sink.write(&[1, 2, 3]).await.unwrap();
        sink.clear();
        assert_eq!(sink.written(), Vec::<i16>::new());
        assert_eq!(sink.clear_count(), 1);
    }
}
