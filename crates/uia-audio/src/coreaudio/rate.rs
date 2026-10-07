// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Raising a slow device's nominal sample rate before voice processing opens.
//!
//! Voice Processing IO runs both directions at the lower of the two devices'
//! nominal rates (measured: a 16 kHz C920 mic with a 48 kHz speaker
//! processes at 16 kHz, cutting the assistant's voice to 8 kHz of
//! bandwidth). So before a unit opens, a bound device below
//! [`FULL_VOICE_RATE_HZ`] that supports more is raised, and put back when
//! the app releases it.

use super::device::{self, DeviceInfo};
use super::{Wake, wait_while};
use objc2_core_audio::AudioDeviceID;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// How long to wait for a raised rate to read back before opening anyway.
const RATE_SETTLE_LIMIT: Duration = Duration::from_millis(500);
/// How often that wait re-checks.
const RATE_SETTLE_POLL: Duration = Duration::from_millis(50);

/// The processing rate at and above which the assistant loses nothing: the
/// engine's voice arrives as 24 kHz PCM (OpenAI Realtime), so it carries at
/// most 12 kHz of content, which a 24 kHz processing rate keeps whole.
pub(super) const FULL_VOICE_RATE_HZ: u32 = 24_000;

/// The highest rate a slow device is raised to.
const MAX_RAISED_RATE_HZ: f64 = 48_000.0;

/// What to raise a device running at `current` Hz to, given its available
/// nominal rates as `(min, max)` ranges (`min == max` for a discrete rate).
///
/// `None` when it already runs at [`FULL_VOICE_RATE_HZ`] or more, or when
/// nothing it supports at or below 48 kHz is faster than `current`.
/// Otherwise the highest such rate: a range contributes its maximum capped
/// at 48 kHz, as long as the cap does not fall below the range's minimum.
pub(super) fn raised_rate(current: f64, available: &[(f64, f64)]) -> Option<f64> {
    if current >= f64::from(FULL_VOICE_RATE_HZ) {
        return None;
    }
    available
        .iter()
        .map(|&(min, max)| (min, max.min(MAX_RAISED_RATE_HZ)))
        .filter(|&(min, capped)| capped >= min)
        .map(|(_, capped)| capped)
        .filter(|&rate| rate > current)
        .reduce(f64::max)
}

/// Whether a device reads back `rate` (nominal rates are whole numbers in
/// practice; this only absorbs float noise).
fn reads_back(id: AudioDeviceID, rate: f64) -> bool {
    device::nominal_rate(id).is_some_and(|now| (now - rate).abs() < 0.5)
}

/// One device this guard raised.
struct Raised {
    id: AudioDeviceID,
    name: String,
    /// The rate it ran at before.
    original: f64,
    /// The rate we set.
    target: f64,
}

/// The rates raised for one voice unit, restored on drop.
///
/// Must be dropped after the `VoiceUnit` it was raised for: the unit holds
/// the device, so restoring first would change the rate under a running
/// unit. Supervisor thread only.
pub(super) struct RateGuard {
    raised: Vec<Raised>,
}

impl RateGuard {
    /// Raise each of `devices` (once per device ID) that runs below
    /// [`FULL_VOICE_RATE_HZ`] and supports more, then wait — at most
    /// `RATE_SETTLE_LIMIT`, waking for nothing but a stop — for the new
    /// rates to read back. Opening goes ahead whatever they read.
    ///
    /// A device that refuses is not fatal: it is logged once per device ID
    /// (tracked in `refused`, which the caller keeps across rebuilds) and the
    /// unit opens at the slower rate. `None` means a stop arrived; anything
    /// already raised has been restored by the time it returns.
    pub(super) fn raise(
        wake: &Receiver<Wake>,
        devices: &[&DeviceInfo],
        refused: &mut Vec<AudioDeviceID>,
    ) -> Option<Self> {
        let mut guard = Self { raised: Vec::new() };
        let mut seen: Vec<AudioDeviceID> = Vec::new();
        for device in devices {
            if seen.contains(&device.id) {
                continue;
            }
            seen.push(device.id);
            let Some(original) = device::nominal_rate(device.id) else {
                continue;
            };
            let Some(target) = raised_rate(original, &device::available_rates(device.id)) else {
                continue;
            };
            match device::set_nominal_rate(device.id, target) {
                Ok(()) => {
                    eprintln!(
                        "uia-audio: coreaudio raised {:?} from {original} Hz to {target} Hz \
                         for voice processing (restored on release)",
                        device.name
                    );
                    guard.raised.push(Raised {
                        id: device.id,
                        name: device.name.clone(),
                        original,
                        target,
                    });
                }
                Err(status) => {
                    if !refused.contains(&device.id) {
                        refused.push(device.id);
                        eprintln!(
                            "uia-audio: coreaudio could not raise {:?} from {original} Hz to \
                             {target} Hz ({}); voice processing will run at the slower rate",
                            device.name,
                            super::unit::describe_status(status)
                        );
                    }
                }
            }
        }
        if guard.raised.is_empty() {
            return Some(guard);
        }
        let pending = || guard.raised.iter().any(|r| !reads_back(r.id, r.target));
        wait_while(wake, RATE_SETTLE_LIMIT, RATE_SETTLE_POLL, pending)?;
        Some(guard)
    }
}

impl Drop for RateGuard {
    /// Put each raised device back — unless it reads neither the rate we set
    /// nor (our change still in flight, e.g. a stop during the settle wait)
    /// its original, which means it is gone or someone else changed it since.
    fn drop(&mut self) {
        for r in &self.raised {
            if !reads_back(r.id, r.target) && !reads_back(r.id, r.original) {
                continue;
            }
            if let Err(status) = device::set_nominal_rate(r.id, r.original) {
                eprintln!(
                    "uia-audio: coreaudio could not restore {:?} to {} Hz ({})",
                    r.name,
                    r.original,
                    super::unit::describe_status(status)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measured: a C920 runs at 16 kHz and offers 16, 24 and 32 kHz.
    #[test]
    fn a_slow_webcam_mic_is_raised_to_its_highest_rate() {
        let c920 = [
            (16_000.0, 16_000.0),
            (24_000.0, 24_000.0),
            (32_000.0, 32_000.0),
        ];
        assert_eq!(raised_rate(16_000.0, &c920), Some(32_000.0));
    }

    #[test]
    fn a_device_with_nothing_faster_is_left_alone() {
        assert_eq!(raised_rate(16_000.0, &[(16_000.0, 16_000.0)]), None);
        assert_eq!(
            raised_rate(16_000.0, &[(8_000.0, 8_000.0), (16_000.0, 16_000.0)]),
            None
        );
    }

    /// Measured: the Jabra speaker runs at 44.1 kHz and offers 48 kHz too —
    /// already fast enough, so untouched.
    #[test]
    fn a_device_already_at_full_voice_rate_is_left_alone() {
        let jabra = [
            (8_000.0, 8_000.0),
            (16_000.0, 16_000.0),
            (32_000.0, 32_000.0),
            (44_100.0, 44_100.0),
            (48_000.0, 48_000.0),
        ];
        assert_eq!(raised_rate(44_100.0, &jabra), None);
        assert_eq!(raised_rate(24_000.0, &jabra), None);
    }

    /// A continuous range is capped at 48 kHz.
    #[test]
    fn a_range_is_capped_at_48k() {
        assert_eq!(raised_rate(8_000.0, &[(8_000.0, 96_000.0)]), Some(48_000.0));
    }

    /// Rates above 48 kHz only are never chosen.
    #[test]
    fn rates_above_48k_are_never_chosen() {
        assert_eq!(
            raised_rate(16_000.0, &[(16_000.0, 16_000.0), (96_000.0, 96_000.0)]),
            None
        );
        assert_eq!(
            raised_rate(16_000.0, &[(16_000.0, 16_000.0), (88_200.0, 192_000.0)]),
            None
        );
    }

    #[test]
    fn no_available_rates_means_no_change() {
        assert_eq!(raised_rate(16_000.0, &[]), None);
    }
}
