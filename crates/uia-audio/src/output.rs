// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use crate::cpal_stream::Leg;
use crate::follow::OpenFailure;
use crate::input::to_audio_format;
use crate::live::{LiveFormat, PlaybackQueue};
use async_trait::async_trait;
use cpal::traits::{DeviceTrait, HostTrait};
use std::sync::Arc;
use uia_core::audio::{AudioError, AudioFormat, AudioSink, WaitingCell};

/// First output device whose name contains `name` (case-insensitive). A miss
/// is [`OpenFailure::MissingPinned`], naming what cpal actually enumerated,
/// rather than a silent fallback to the default: a pinned device is waited
/// for, and the list makes a config typo easy to spot in the log.
pub(crate) fn find_output_device_by_name(
    host: &cpal::Host,
    name: &str,
) -> Result<cpal::Device, OpenFailure> {
    let mut available = Vec::new();
    for device in host.output_devices().map_err(|e| {
        OpenFailure::Other(AudioError::DeviceUnavailable(format!(
            "cpal: could not list outputs: {e}"
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
        detail: format!("cpal: no output device matching {name:?} among: {available:?}"),
    })
}

/// Names of every output device cpal currently enumerates, in host order —
/// for a settings UI's device picker, not for opening a stream. Matches
/// exactly what [`find_output_device_by_name`]'s substring match and the
/// app's own startup log line show, so a name copied from here always
/// resolves. Repeated names appear once (see `crate::dedupe_names` for why).
pub fn list_output_devices() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|e| AudioError::DeviceUnavailable(format!("cpal: could not list outputs: {e}")))?;
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

/// Speaker playback over cpal, exposed as an `AudioSink`.
///
/// Written frames land in a shared [`PlaybackQueue`]; the cpal output
/// callback drains it on the realtime thread (padding with silence if starved
/// rather than blocking). `clear()` empties the queue synchronously and
/// non-blocking — a plain mutex lock, never an await — which is what makes
/// barge-in work: the frozen decision in PLAN.md requires the sink to clear
/// within 50 ms of detected speech, independent of the engine's `interrupt()`.
///
/// The stream underneath is reopened by its [`Leg`] when the device is lost
/// or, for the default output, when the OS default moves; the queue survives
/// a reopen at the same rate.
pub struct CpalSink {
    format: Arc<LiveFormat>,
    queue: PlaybackQueue,
    waiting: WaitingCell,
    // Held only to keep the stream alive; dropping it stops playback.
    _leg: Leg,
}

impl CpalSink {
    pub fn default_output() -> Result<Self, AudioError> {
        Self::output(None)
    }

    /// Open the named output device, or the default one if `device_name` is
    /// `None`. Matching is case-insensitive substring, same as the WASAPI
    /// path's `com::find_device_by_name` — a user can paste the name Windows
    /// shows without worrying about exact punctuation. A configured name that
    /// matches nothing is never replaced by the default: the sink opens
    /// waiting for it, reports that through [`AudioSink::waiting`], and
    /// starts playing once it appears.
    pub fn output(device_name: Option<&str>) -> Result<Self, AudioError> {
        let format = Arc::new(LiveFormat::new());
        let queue = PlaybackQueue::default();
        let waiting = WaitingCell::default();
        let leg = Leg::render(
            device_name,
            queue.clone(),
            Arc::clone(&format),
            waiting.clone(),
        )?;
        Ok(Self {
            format,
            queue,
            waiting,
            _leg: leg,
        })
    }

    /// Number of samples currently queued for playback. Exposed for tests;
    /// production code has no need to poll it.
    pub fn buffered_len(&self) -> usize {
        self.queue.len()
    }
}

#[async_trait]
impl AudioSink for CpalSink {
    fn format(&self) -> AudioFormat {
        let (sample_rate_hz, channels) = self.format.load();
        to_audio_format(sample_rate_hz, channels)
    }

    /// Dropped while waiting for a device: nothing would play it, and a
    /// backlog would come out late, at whatever rate the device turns up at.
    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError> {
        if self.waiting.get().is_none() {
            self.queue.push(frame);
        }
        Ok(())
    }

    fn clear(&mut self) {
        self.queue.clear();
    }

    fn level(&self) -> f32 {
        self.queue.level()
    }

    fn waiting(&self) -> Option<WaitingCell> {
        Some(self.waiting.clone())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unmatched_device_name_is_a_clear_error_not_a_silent_fallback() {
        let host = cpal::default_host();
        let err = super::find_output_device_by_name(&host, "definitely-not-a-real-device-9f3a1c")
            .unwrap_err();
        match err {
            crate::follow::OpenFailure::MissingPinned { name, detail } => {
                assert_eq!(name, "definitely-not-a-real-device-9f3a1c");
                assert!(
                    detail.contains("no output device matching"),
                    "got: {detail}"
                );
            }
            other => panic!("expected a missing pinned device, got {other:?}"),
        }
    }

    /// Needs no device, like its input twin.
    #[test]
    fn a_pinned_output_that_is_missing_at_open_waits_instead_of_failing() {
        use uia_core::audio::{AudioSink, WaitingFor};
        let sink = super::CpalSink::output(Some("definitely-not-a-real-device-9f3a1c"))
            .expect("a missing pinned output opens in the waiting state");
        assert_eq!(
            sink.waiting().unwrap().get(),
            Some(WaitingFor::Named(
                "definitely-not-a-real-device-9f3a1c".into()
            ))
        );
    }

    #[tokio::test]
    #[ignore = "requires a real audio device; run manually on Windows"]
    async fn default_output_device_opens() {
        use uia_core::audio::AudioSink;
        let sink = super::CpalSink::default_output().unwrap();
        assert!(sink.format().sample_rate_hz > 0);
    }

    #[tokio::test]
    #[ignore = "requires a real audio device; run manually on Windows"]
    async fn clear_empties_the_buffer_synchronously() {
        use uia_core::audio::AudioSink;
        let mut sink = super::CpalSink::default_output().unwrap();
        sink.write(&[1, 2, 3, 4]).await.unwrap();
        sink.clear();
        assert_eq!(sink.buffered_len(), 0);
    }
}
