// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Platform-free core: ports, conversation state machine, audio primitives.
//!
//! This crate must never depend on `cpal`, `tauri`, `tokio-tungstenite`, or any
//! `aws-sdk-*` crate. Enforced by `scripts/check-core-deps.sh`.

pub mod activation;
pub mod audio;
pub mod creds;
pub mod engine;
pub mod memory;
pub mod session;
pub mod tools;
