// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Pcm16Le,
}

/// The wire format an engine or device expects. Engines declare their own
/// rates rather than the pipeline hardcoding them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub encoding: Encoding,
}

impl AudioFormat {
    pub const fn mono_pcm16(sample_rate_hz: u32) -> Self {
        Self {
            sample_rate_hz,
            channels: 1,
            encoding: Encoding::Pcm16Le,
        }
    }
}
