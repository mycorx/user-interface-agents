// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use async_trait::async_trait;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;
use uia_core::audio::{
    AudioError, AudioFormat, AudioSource, Encoding, downmix_to_mono, f32_to_i16,
};

/// Translate a cpal stream's rate/channel-count into our own `AudioFormat`.
/// Device-independent, so it is the one part of this crate CI can assert on.
pub fn to_audio_format(sample_rate_hz: u32, channels: u16) -> AudioFormat {
    AudioFormat {
        sample_rate_hz,
        channels,
        encoding: Encoding::Pcm16Le,
    }
}

/// First input device whose name contains `name` (case-insensitive). Fails
/// loudly with the list of what cpal actually enumerated on a miss, rather
/// than silently falling back to the default — a config typo deserves an
/// error naming the available devices, not a debugging session.
fn find_input_device_by_name(host: &cpal::Host, name: &str) -> Result<cpal::Device, AudioError> {
    let mut available = Vec::new();
    for device in host
        .input_devices()
        .map_err(|e| AudioError::DeviceUnavailable(format!("cpal: could not list inputs: {e}")))?
    {
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown device>".into());
        if crate::device_name_matches(&device_name, name) {
            return Ok(device);
        }
        available.push(device_name);
    }
    Err(AudioError::DeviceUnavailable(format!(
        "cpal: no input device matching {name:?} among: {available:?}"
    )))
}

/// Names of every input device cpal currently enumerates, in host order — for
/// a settings UI's device picker, not for opening a stream. Matches exactly
/// what [`find_input_device_by_name`]'s substring match and the app's own
/// startup log line show, so a name copied from here always resolves.
pub fn list_input_devices() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|e| AudioError::DeviceUnavailable(format!("cpal: could not list inputs: {e}")))?;
    Ok(devices
        .map(|d| {
            d.description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| "<unknown device>".into())
        })
        .collect())
}

/// Microphone capture over cpal, exposed as an `AudioSource`.
///
/// The realtime capture callback never blocks and never awaits: it converts
/// each buffer to i16 and `try_send`s it, dropping the frame on backpressure
/// rather than stalling the audio thread. `next_frame` is where backpressure
/// is felt instead, by a caller that can afford to wait.
pub struct CpalSource {
    format: AudioFormat,
    rx: mpsc::Receiver<Vec<i16>>,
    // Held only to keep the stream alive; cpal stops capture on drop.
    _stream: cpal::Stream,
}

impl CpalSource {
    pub fn default_input() -> Result<Self, AudioError> {
        Self::input(None)
    }

    /// Open the named input device, or the default one if `device_name` is
    /// `None`. Matching is case-insensitive substring, same as the WASAPI
    /// path's `com::find_device_by_name` — a user can paste the name Windows
    /// shows without worrying about exact punctuation. A configured name that
    /// matches nothing is a clear error rather than a silent fallback to the
    /// default, listing what cpal actually sees so a typo is easy to spot.
    pub fn input(device_name: Option<&str>) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => find_input_device_by_name(&host, name)?,
            None => host
                .default_input_device()
                .ok_or_else(|| AudioError::DeviceUnavailable("no default input device".into()))?,
        };
        let config = device
            .default_input_config()
            .map_err(|e| AudioError::DeviceUnavailable(e.to_string()))?;
        // Every frame this source yields is downmixed to mono below, so the
        // format it advertises is mono regardless of what the device
        // actually captures — a stereo default device (the Windows norm) is
        // otherwise a silent lie that leaves every consumer (the resampler,
        // the Opus encoder, which rejects a wrongly-sized frame outright)
        // treating interleaved L/R samples as if they were one channel.
        let device_channels = config.channels();
        let format = to_audio_format(config.sample_rate(), 1);

        // Bounded and small: this is live audio, not a backlog to catch up on.
        // A full channel means the consumer has fallen behind, and the right
        // move is to drop the newest frame, not to buffer indefinitely.
        let (tx, rx) = mpsc::channel(64);
        let stream_config: cpal::StreamConfig = config.into();
        let stream = device
            .build_input_stream(
                stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let pcm = f32_to_i16(data);
                    let mono = downmix_to_mono(&pcm, device_channels);
                    let _ = tx.try_send(mono);
                },
                move |err| eprintln!("uia-audio: input stream error: {err}"),
                None,
            )
            .map_err(|e| AudioError::DeviceUnavailable(e.to_string()))?;
        stream
            .play()
            .map_err(|e| AudioError::DeviceUnavailable(e.to_string()))?;

        Ok(Self {
            format,
            rx,
            _stream: stream,
        })
    }
}

#[async_trait]
impl AudioSource for CpalSource {
    fn format(&self) -> AudioFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        self.rx.recv().await
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
        let msg = err.to_string();
        assert!(msg.contains("no input device matching"), "got: {msg}");
    }

    #[test]
    fn cpal_sample_rate_maps_to_our_audio_format() {
        let f = super::to_audio_format(48_000, 2);
        assert_eq!(f.sample_rate_hz, 48_000);
        assert_eq!(f.channels, 2);
        assert_eq!(f.encoding, Encoding::Pcm16Le);
    }

    #[tokio::test]
    #[ignore = "requires a real audio device; run manually on Windows"]
    async fn default_input_device_opens() {
        use uia_core::audio::AudioSource;
        let src = super::CpalSource::default_input().unwrap();
        assert!(src.format().sample_rate_hz > 0);
    }
}
