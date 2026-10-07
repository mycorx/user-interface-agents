// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! How long the OS echo-cancellation backends wait between attempts to reopen
//! a device that has gone away.
//!
//! Shared by `wasapi` and `coreaudio` so "patient, never gives up" means the
//! same thing on both platforms. Compiled everywhere, so its tests run in the
//! Linux CI that can build neither backend.

/// Delays between attempts to reopen a device that has gone away, in
/// milliseconds; the last repeats for as long as it takes.
///
/// The tail is deliberately patient rather than a giving-up point. The case
/// this exists for is a KVM switch or a dock: the microphone is handed to
/// another machine and comes back minutes or hours later, and the right
/// behaviour is to still be there when it does. Retrying forever at five
/// second intervals costs one device enumeration per tick; giving up costs
/// the user an app that looks fine and cannot hear them.
#[cfg_attr(
    not(any(
        all(windows, feature = "wasapi-aec"),
        all(target_os = "macos", feature = "coreaudio-aec")
    )),
    allow(dead_code)
)]
pub(crate) const REOPEN_DELAYS_MS: [u32; 5] = [200, 500, 1_000, 2_000, 5_000];

/// The delay for a given attempt, saturating at the last entry.
///
/// Its own function so the clamp is testable without a device: an
/// out-of-bounds index here would panic on an audio thread and take the
/// device down permanently, which is precisely the failure the reopen loop
/// exists to prevent. `attempt` counts up without limit for a device that
/// never returns.
#[cfg_attr(
    not(any(
        all(windows, feature = "wasapi-aec"),
        all(target_os = "macos", feature = "coreaudio-aec")
    )),
    allow(dead_code)
)]
pub(crate) fn reopen_delay_ms(attempt: u32) -> u32 {
    REOPEN_DELAYS_MS[(attempt as usize).min(REOPEN_DELAYS_MS.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reopen loop counts attempts up without bound while a device stays
    /// away, so the delay lookup must saturate rather than index past its
    /// table. A panic here happens on the audio thread and kills the device
    /// for the life of the process — the exact failure the loop exists to fix.
    #[test]
    fn the_reopen_delay_saturates_instead_of_indexing_past_the_table() {
        assert_eq!(reopen_delay_ms(0), REOPEN_DELAYS_MS[0]);
        let last = REOPEN_DELAYS_MS[REOPEN_DELAYS_MS.len() - 1];
        for attempt in [
            REOPEN_DELAYS_MS.len() as u32 - 1,
            REOPEN_DELAYS_MS.len() as u32,
            1_000,
            u32::MAX,
        ] {
            assert_eq!(reopen_delay_ms(attempt), last, "attempt {attempt}");
        }
    }

    /// The delays must grow, or a device that is genuinely gone is enumerated
    /// at the fastest rate forever.
    #[test]
    fn the_reopen_delays_back_off() {
        assert!(
            REOPEN_DELAYS_MS.windows(2).all(|w| w[0] < w[1]),
            "delays must increase: {REOPEN_DELAYS_MS:?}"
        );
    }
}
