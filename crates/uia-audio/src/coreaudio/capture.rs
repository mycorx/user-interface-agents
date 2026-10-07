// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The echo-cancelled microphone, exposed as an `AudioSource`.
//!
//! Shaped like `crate::input::CpalSource`: a bounded channel fed by a
//! callback that never blocks, drained by `next_frame`.

use super::{Backend, app_format};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;
use uia_core::audio::{AudioFormat, AudioSource};

pub struct CoreAudioSource {
    rx: mpsc::Receiver<Vec<i16>>,
    _backend: Arc<Backend>,
}

impl CoreAudioSource {
    pub(super) fn new(rx: mpsc::Receiver<Vec<i16>>, backend: Arc<Backend>) -> Self {
        Self {
            rx,
            _backend: backend,
        }
    }
}

#[async_trait]
impl AudioSource for CoreAudioSource {
    fn format(&self) -> AudioFormat {
        app_format()
    }

    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        self.rx.recv().await
    }
}
