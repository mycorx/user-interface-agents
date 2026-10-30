// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The echo-cancelled microphone, exposed as an `AudioSource`.
//!
//! Shaped like `crate::input::CpalSource`: a bounded channel fed by a
//! callback that never blocks, drained by `next_frame`. While the unit is
//! split, a plain cpal leg feeds the same channel.

use super::Backend;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;
use uia_core::audio::{AudioFormat, AudioSource, WaitingCell};

pub struct CoreAudioSource {
    rx: mpsc::Receiver<Vec<i16>>,
    backend: Arc<Backend>,
}

impl CoreAudioSource {
    pub(super) fn new(rx: mpsc::Receiver<Vec<i16>>, backend: Arc<Backend>) -> Self {
        Self { rx, backend }
    }
}

#[async_trait]
impl AudioSource for CoreAudioSource {
    /// Always mono: the unit records mono and a split leg downmixes.
    fn format(&self) -> AudioFormat {
        let (sample_rate_hz, _) = self.backend.shared.input_format.load();
        crate::input::to_audio_format(sample_rate_hz, 1)
    }

    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        self.rx.recv().await
    }

    fn waiting(&self) -> Option<WaitingCell> {
        Some(self.backend.shared.input_waiting.clone())
    }
}
