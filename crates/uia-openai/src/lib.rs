// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! OpenAI Realtime `S2sEngine` over native WebRTC.

pub mod creds;
pub mod engine;

pub use engine::{DEFAULT_MODEL, OpenAiEngine};
pub mod protocol;
