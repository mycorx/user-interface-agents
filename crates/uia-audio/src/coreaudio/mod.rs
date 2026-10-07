// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! macOS-native echo cancellation, via the Voice Processing IO audio unit.

mod device;
#[allow(dead_code)] // used by the supervisor (Task 4)
mod unit;

/// What wakes the supervisor thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// A CoreAudio property the supervisor listens to changed.
    Changed,
    /// The backend is being dropped.
    #[allow(dead_code)] // used by the supervisor (Task 4)
    Stop,
}
