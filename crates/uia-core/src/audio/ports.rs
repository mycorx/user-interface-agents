// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use crate::audio::AudioFormat;
use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("audio device unavailable: {0}")]
    DeviceUnavailable(String),
    #[error("resampling failed: {0}")]
    Resample(String),
    #[error("invalid sample rate: {from_hz} -> {to_hz}")]
    InvalidRate { from_hz: u32, to_hz: u32 },
}

#[async_trait]
pub trait AudioSource: Send {
    fn format(&self) -> AudioFormat;
    /// Yields the next captured frame, or `None` when the source is exhausted.
    async fn next_frame(&mut self) -> Option<Vec<i16>>;
}

#[async_trait]
pub trait AudioSink: Send {
    fn format(&self) -> AudioFormat;
    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError>;
    /// Drop all buffered audio immediately. Synchronous and non-blocking by
    /// contract: barge-in must not await anything, least of all the network.
    fn clear(&mut self);
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

/// A handle onto a `VecSink`'s observations that survives the sink being moved
/// into a `Session`. Cloning shares the same underlying buffer, so a test can
/// keep watching a sink it no longer owns.
#[derive(Clone, Default)]
pub struct SinkProbe {
    buf: std::sync::Arc<std::sync::Mutex<Vec<i16>>>,
    clears: std::sync::Arc<std::sync::Mutex<usize>>,
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
        Ok(())
    }
    fn clear(&mut self) {
        self.probe.buf.lock().unwrap().clear();
        *self.probe.clears.lock().unwrap() += 1;
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
