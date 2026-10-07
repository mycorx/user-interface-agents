// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use crate::input::to_audio_format;
use async_trait::async_trait;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use uia_core::audio::{AudioError, AudioFormat, AudioSink};

/// First output device whose name contains `name` (case-insensitive). Fails
/// loudly with the list of what cpal actually enumerated on a miss, rather
/// than silently falling back to the default — a config typo deserves an
/// error naming the available devices, not a debugging session.
fn find_output_device_by_name(host: &cpal::Host, name: &str) -> Result<cpal::Device, AudioError> {
    let mut available = Vec::new();
    for device in host
        .output_devices()
        .map_err(|e| AudioError::DeviceUnavailable(format!("cpal: could not list outputs: {e}")))?
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
        "cpal: no output device matching {name:?} among: {available:?}"
    )))
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
/// Written frames land in a shared ring buffer; the cpal output callback
/// drains it on the realtime thread (padding with silence if starved rather
/// than blocking). `clear()` empties the buffer synchronously and
/// non-blocking — a plain mutex lock, never an await — which is what makes
/// barge-in work: the frozen decision in PLAN.md requires the sink to clear
/// within 50 ms of detected speech, independent of the engine's `interrupt()`.
pub struct CpalSink {
    format: AudioFormat,
    buf: Arc<Mutex<VecDeque<i16>>>,
    // Held only to keep the stream alive; cpal stops playback on drop.
    _stream: cpal::Stream,
}

impl CpalSink {
    pub fn default_output() -> Result<Self, AudioError> {
        Self::output(None)
    }

    /// Open the named output device, or the default one if `device_name` is
    /// `None`. Matching is case-insensitive substring, same as the WASAPI
    /// path's `com::find_device_by_name` — a user can paste the name Windows
    /// shows without worrying about exact punctuation. A configured name that
    /// matches nothing is a clear error rather than a silent fallback to the
    /// default, listing what cpal actually sees so a typo is easy to spot.
    pub fn output(device_name: Option<&str>) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => find_output_device_by_name(&host, name)?,
            None => host
                .default_output_device()
                .ok_or_else(|| AudioError::DeviceUnavailable("no default output device".into()))?,
        };
        let config = device
            .default_output_config()
            .map_err(|e| AudioError::DeviceUnavailable(e.to_string()))?;
        let format = to_audio_format(config.sample_rate(), config.channels());

        let buf: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let callback_buf = Arc::clone(&buf);
        // The engine speaks mono; `write` queues one sample per audio frame.
        // A stereo (or wider) device's `data` interleaves N slots per frame,
        // so each queued sample must fan out to every slot in its frame —
        // popping one queued sample per *slot* instead silently plays two
        // mono samples across L/R, which halves playback duration and pitches
        // the voice up exactly like an unresampled rate mismatch does.
        let channels = config.channels().max(1) as usize;
        let stream_config: cpal::StreamConfig = config.into();
        let stream = device
            .build_output_stream(
                stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut queue = callback_buf.lock().unwrap();
                    for frame in data.chunks_mut(channels) {
                        let sample = match queue.pop_front() {
                            Some(s) => s as f32 / 32768.0,
                            // Starved: pad with silence rather than block.
                            None => 0.0,
                        };
                        for slot in frame {
                            *slot = sample;
                        }
                    }
                },
                move |err| eprintln!("uia-audio: output stream error: {err}"),
                None,
            )
            .map_err(|e| AudioError::DeviceUnavailable(e.to_string()))?;
        stream
            .play()
            .map_err(|e| AudioError::DeviceUnavailable(e.to_string()))?;

        Ok(Self {
            format,
            buf,
            _stream: stream,
        })
    }

    /// Number of samples currently queued for playback. Exposed for tests;
    /// production code has no need to poll it.
    pub fn buffered_len(&self) -> usize {
        self.buf.lock().unwrap().len()
    }
}

#[async_trait]
impl AudioSink for CpalSink {
    fn format(&self) -> AudioFormat {
        self.format
    }

    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError> {
        self.buf.lock().unwrap().extend(frame.iter().copied());
        Ok(())
    }

    fn clear(&mut self) {
        self.buf.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unmatched_device_name_is_a_clear_error_not_a_silent_fallback() {
        let host = cpal::default_host();
        let err = super::find_output_device_by_name(&host, "definitely-not-a-real-device-9f3a1c")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no output device matching"), "got: {msg}");
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
