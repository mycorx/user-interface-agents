// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Speaker playback through the Voice Processing unit, exposed as an
//! `AudioSink`. What plays here is the echo reference, so the assistant's
//! voice must go out this way for the microphone side to cancel it.
//!
//! Same queue shape as `crate::output::CpalSink` and `WasapiSink`: `write`
//! appends, the render callback drains, `clear()` empties synchronously.
//! After a `clear()` at most one callback period still plays — about 11 ms at
//! 48 kHz, about 36 ms with a 16 kHz device — inside the 50 ms barge-in budget.
//! While the unit is split, a plain cpal leg drains the same queue, without
//! echo cancellation.

use super::Backend;
use async_trait::async_trait;
use std::sync::Arc;
use uia_core::audio::{AudioError, AudioFormat, AudioSink, WaitingCell};

pub struct CoreAudioSink {
    pub(super) backend: Arc<Backend>,
}

impl CoreAudioSink {
    pub(super) fn new(backend: Arc<Backend>) -> Self {
        Self { backend }
    }

    /// Number of samples currently queued for playback. Exposed for tests;
    /// production code has no need to poll it.
    pub fn buffered_len(&self) -> usize {
        self.backend.shared.playback.len()
    }
}

#[async_trait]
impl AudioSink for CoreAudioSink {
    fn format(&self) -> AudioFormat {
        let (sample_rate_hz, channels) = self.backend.shared.output_format.load();
        crate::input::to_audio_format(sample_rate_hz, channels)
    }

    /// Dropped while waiting for a device, as `CpalSink` does: nothing would
    /// play it, and a backlog would come out late.
    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError> {
        let shared = &self.backend.shared;
        if shared.output_waiting.get().is_none() {
            shared.playback.push(frame);
        }
        Ok(())
    }

    fn clear(&mut self) {
        // Synchronous and non-blocking, per the `AudioSink` contract.
        self.backend.shared.playback.clear();
    }

    fn level(&self) -> f32 {
        self.backend.shared.playback.level()
    }

    fn waiting(&self) -> Option<WaitingCell> {
        Some(self.backend.shared.output_waiting.clone())
    }
}
