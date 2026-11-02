// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use crate::cpal_stream::Leg;
use crate::follow::OpenFailure;
use crate::live::LiveFormat;
use async_trait::async_trait;
use cpal::traits::{DeviceTrait, HostTrait};
use std::sync::Arc;
use tokio::sync::mpsc;
use uia_core::audio::{AudioError, AudioFormat, AudioSource, Encoding, WaitingCell};

/// Translate a cpal stream's rate/channel-count into our own `AudioFormat`.
/// Device-independent, so it is the one part of this crate CI can assert on.
pub fn to_audio_format(sample_rate_hz: u32, channels: u16) -> AudioFormat {
    AudioFormat {
        sample_rate_hz,
        channels,
        encoding: Encoding::Pcm16Le,
    }
}

/// First input device whose name contains `name` (case-insensitive). A miss
/// is [`OpenFailure::MissingPinned`], naming what cpal actually enumerated,
/// rather than a silent fallback to the default: a pinned device is waited
/// for, and the list makes a config typo easy to spot in the log.
pub(crate) fn find_input_device_by_name(
    host: &cpal::Host,
    name: &str,
) -> Result<cpal::Device, OpenFailure> {
    let mut available = Vec::new();
    for device in host.input_devices().map_err(|e| {
        OpenFailure::Other(AudioError::DeviceUnavailable(format!(
            "cpal: could not list inputs: {e}"
        )))
    })? {
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown device>".into());
        if crate::device_name_matches(&device_name, name) {
            return Ok(device);
        }
        available.push(device_name);
    }
    Err(OpenFailure::MissingPinned {
        name: name.to_string(),
        detail: format!("cpal: no input device matching {name:?} among: {available:?}"),
    })
}

/// Names of every input device cpal currently enumerates, in host order — for
/// a settings UI's device picker, not for opening a stream. Matches exactly
/// what [`find_input_device_by_name`]'s substring match and the app's own
/// startup log line show, so a name copied from here always resolves.
/// Repeated names appear once (see `crate::dedupe_names` for why).
pub fn list_input_devices() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|e| AudioError::DeviceUnavailable(format!("cpal: could not list inputs: {e}")))?;
    Ok(crate::dedupe_names(
        devices
            .map(|d| {
                d.description()
                    .map(|d| d.name().to_string())
                    .unwrap_or_else(|_| "<unknown device>".into())
            })
            .collect(),
    ))
}

/// Microphone capture over cpal, exposed as an `AudioSource`.
///
/// The realtime capture callback never blocks and never awaits: it converts
/// each buffer to i16 and `try_send`s it, dropping the frame on backpressure
/// rather than stalling the audio thread. `next_frame` is where backpressure
/// is felt instead, by a caller that can afford to wait.
///
/// The stream underneath is reopened by its [`Leg`] when the device is lost
/// or, for the default input, when the OS default moves; frames keep arriving
/// on the same channel, at whatever rate `format` reports.
pub struct CpalSource {
    format: Arc<LiveFormat>,
    waiting: WaitingCell,
    rx: mpsc::Receiver<Vec<i16>>,
    // Held only to keep the stream alive; dropping it stops capture.
    _leg: Leg,
}

impl CpalSource {
    pub fn default_input() -> Result<Self, AudioError> {
        Self::input(None)
    }

    /// Open the named input device, or the default one if `device_name` is
    /// `None`. Matching is case-insensitive substring, same as the WASAPI
    /// path's `com::find_device_by_name` — a user can paste the name Windows
    /// shows without worrying about exact punctuation. A configured name that
    /// matches nothing is never replaced by the default: the source opens
    /// waiting for it, reports that through [`AudioSource::waiting`], and
    /// starts capturing once it appears.
    pub fn input(device_name: Option<&str>) -> Result<Self, AudioError> {
        // Bounded and small: this is live audio, not a backlog to catch up on.
        // A full channel means the consumer has fallen behind, and the right
        // move is to drop the newest frame, not to buffer indefinitely.
        let (tx, rx) = mpsc::channel(64);
        let format = Arc::new(LiveFormat::new());
        let waiting = WaitingCell::default();
        let leg = Leg::capture(device_name, tx, Arc::clone(&format), waiting.clone())?;
        Ok(Self {
            format,
            waiting,
            rx,
            _leg: leg,
        })
    }
}

#[async_trait]
impl AudioSource for CpalSource {
    /// Every frame is downmixed to mono, so this is mono whatever the device
    /// captures — a stereo default device (the Windows norm) is otherwise a
    /// silent lie that leaves every consumer (the resampler, the Opus
    /// encoder, which rejects a wrongly-sized frame outright) treating
    /// interleaved L/R samples as one channel.
    fn format(&self) -> AudioFormat {
        let (sample_rate_hz, _) = self.format.load();
        to_audio_format(sample_rate_hz, 1)
    }

    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        self.rx.recv().await
    }

    fn waiting(&self) -> Option<WaitingCell> {
        Some(self.waiting.clone())
    }
}

#[cfg(test)]
mod tests {
    use uia_core::audio::Encoding;

    #[test]
    fn an_unmatched_device_name_is_a_clear_error_not_a_silent_fallback() {
        // Doesn't need real hardware to be meaningful: whatever inputs this
        // machine has (zero or more), a name this unlikely to collide must
        // never match one, so the miss path is exercised deterministically
        // in CI.
        let host = cpal::default_host();
        let err = super::find_input_device_by_name(&host, "definitely-not-a-real-device-9f3a1c")
            .unwrap_err();
        match err {
            crate::follow::OpenFailure::MissingPinned { name, detail } => {
                assert_eq!(name, "definitely-not-a-real-device-9f3a1c");
                assert!(detail.contains("no input device matching"), "got: {detail}");
            }
            other => panic!("expected a missing pinned device, got {other:?}"),
        }
    }

    #[test]
    fn cpal_sample_rate_maps_to_our_audio_format() {
        let f = super::to_audio_format(48_000, 2);
        assert_eq!(f.sample_rate_hz, 48_000);
        assert_eq!(f.channels, 2);
        assert_eq!(f.encoding, Encoding::Pcm16Le);
    }

    /// Needs no device: a name this unlikely matches nothing on any machine,
    /// including a CI runner with no sound card at all.
    #[test]
    fn a_pinned_input_that_is_missing_at_open_waits_instead_of_failing() {
        use uia_core::audio::{AudioSource, WaitingFor};
        let src = super::CpalSource::input(Some("definitely-not-a-real-device-9f3a1c"))
            .expect("a missing pinned input opens in the waiting state");
        assert_eq!(
            src.waiting().unwrap().get(),
            Some(WaitingFor::Named(
                "definitely-not-a-real-device-9f3a1c".into()
            ))
        );
    }

    /// The session drops a waiting pair at every Settings close, so a leg in
    /// the middle of a backoff must not hold that up for the rest of it.
    #[test]
    fn dropping_a_leg_that_waits_for_a_missing_device_returns_promptly() {
        let src = super::CpalSource::input(Some("definitely-not-a-real-device-9f3a1c")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let started = std::time::Instant::now();
        drop(src);
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(200),
            "drop took {took:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires a real audio device; run manually on Windows"]
    async fn default_input_device_opens() {
        use uia_core::audio::AudioSource;
        let src = super::CpalSource::default_input().unwrap();
        assert!(src.format().sample_rate_hz > 0);
    }
}
