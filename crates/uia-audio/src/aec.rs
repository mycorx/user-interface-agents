// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Capture-side echo cancellation, wrapping `webrtc-audio-processing` 2.1.0.
//!
//! Frozen decision (PLAN.md): echo cancellation lives in capture, not in the
//! transport. The Rust `webrtc` crate carries no AEC/NS/AGC, so without this
//! module the assistant's own speaker output bleeds into the microphone and
//! the barge-in RMS detector in `uia-core::session` fires on our own
//! voice. This benefits both engines because it sits before any transport.
//!
//! Gated behind the `aec` Cargo feature (default OFF): the crate links a
//! bundled C++ library, which needs meson + ninja + a C++ compiler + libclang
//! (for bindgen) on `PATH` - none of which this WSL2 sandbox has out of the
//! box, mirroring the `desktop`-feature precedent `uia-app` uses for
//! `tauri`. `cargo test -p uia-audio` (no flags) never touches this
//! module; build/test with `--features aec` once the toolchain is present.
//! See LEDGER.md's S15 notes for exactly what was needed and how it was
//! obtained without root.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use uia_core::audio::AudioError;
use webrtc_audio_processing::config::EchoCanceller as Aec3EchoCanceller;
use webrtc_audio_processing::{Config, Processor};

/// Runtime configuration for the capture-side echo canceller. Constructed
/// from `uia.toml` (a future config stage's job); defaults are chosen so
/// an unconfigured install still gets echo cancellation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoCancellerConfig {
    /// Default-on. The documented escape hatch for headset users whose mic
    /// physically cannot pick up the speaker: set this to `false` in config
    /// to skip the CPU cost entirely.
    pub enabled: bool,
    /// Sample rate the processor runs at, in Hz. Must be a multiple of 100 -
    /// the library processes fixed 10ms frames - and no higher than 48_000,
    /// its highest supported internal rate. This is the capture device's
    /// native rate, taken before any resampling toward an engine's rate.
    pub sample_rate_hz: u32,
}

impl Default for EchoCancellerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_rate_hz: 48_000,
        }
    }
}

/// Wraps `webrtc-audio-processing`'s `Processor` behind our own type so the
/// dependency stays swappable and never leaks past this module -
/// `uia-core` never depends on `webrtc-audio-processing` directly.
///
/// The underlying processor works in fixed 10ms frames (`sample_rate_hz /
/// 100` samples); this type buffers both the near-end (capture) and far-end
/// (render/playback reference) streams so callers can feed it whatever
/// buffer size their device or engine hands them, mirroring
/// `uia_core::audio::Resampler`'s streaming contract: a call may return
/// fewer samples than it was given while a partial frame is buffered.
pub struct EchoCanceller {
    processor: Option<Processor>,
    frame_len: usize,
    render_buf: VecDeque<i16>,
    capture_buf: VecDeque<i16>,
    output_buf: VecDeque<i16>,
}

fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / 32768.0
}

fn f32_to_i16(sample: f32) -> i16 {
    (sample * 32768.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

impl EchoCanceller {
    /// Builds a new echo canceller. When `cfg.enabled` is `false` this still
    /// constructs successfully but never touches the underlying library -
    /// `process_capture_frame` becomes a pass-through and `push_render_frame`
    /// a no-op, so callers do not need to branch on the config themselves.
    pub fn new(cfg: EchoCancellerConfig) -> Result<Self, AudioError> {
        if !cfg.enabled {
            return Ok(Self {
                processor: None,
                frame_len: 0,
                render_buf: VecDeque::new(),
                capture_buf: VecDeque::new(),
                output_buf: VecDeque::new(),
            });
        }

        let processor = Processor::new(cfg.sample_rate_hz)
            .map_err(|e| AudioError::DeviceUnavailable(format!("webrtc-audio-processing: {e}")))?;
        processor.set_config(Config {
            echo_canceller: Some(Aec3EchoCanceller::default()),
            ..Default::default()
        });
        let frame_len = processor.num_samples_per_frame();

        Ok(Self {
            processor: Some(processor),
            frame_len,
            render_buf: VecDeque::new(),
            capture_buf: VecDeque::new(),
            output_buf: VecDeque::new(),
        })
    }

    /// Feeds the far-end (playback) reference. AEC cannot work without this -
    /// it is the step PLAN.md calls out as "most likely to be missed, because
    /// everything compiles fine without it and only fails acoustically."
    /// Buffered internally into 10ms frames; a no-op when disabled.
    pub fn push_render_frame(&mut self, far_end: &[i16]) {
        if self.processor.is_none() {
            return;
        }
        self.render_buf.extend(far_end.iter().copied());
    }

    /// Processes one capture buffer of arbitrary length and returns the
    /// echo-cancelled samples that are ready now. May return fewer samples
    /// than `near_end.len()` (or none) while a partial 10ms frame is
    /// buffered; never blocks. When disabled, returns `near_end` unchanged.
    ///
    /// If the render buffer has under-run - nothing queued for playback yet -
    /// the far-end reference for that frame is padded with silence rather
    /// than stalling capture on it.
    pub fn process_capture_frame(&mut self, near_end: &[i16]) -> Vec<i16> {
        let Some(processor) = self.processor.as_ref() else {
            return near_end.to_vec();
        };

        self.capture_buf.extend(near_end.iter().copied());

        while self.capture_buf.len() >= self.frame_len {
            let capture_chunk: Vec<f32> = self
                .capture_buf
                .drain(..self.frame_len)
                .map(i16_to_f32)
                .collect();
            let render_chunk: Vec<f32> = (0..self.frame_len)
                .map(|_| i16_to_f32(self.render_buf.pop_front().unwrap_or(0)))
                .collect();

            let render_frame = vec![render_chunk];
            let mut capture_frame = vec![capture_chunk];

            // The far-end reference for this frame must be analyzed before
            // the matching capture frame is processed against it.
            let _ = processor.analyze_render_frame(&render_frame);
            let _ = processor.process_capture_frame(&mut capture_frame);

            self.output_buf
                .extend(capture_frame[0].iter().copied().map(f32_to_i16));
        }

        self.output_buf.drain(..).collect()
    }
}

/// Wraps a capture `AudioSource` so every frame it yields has already passed
/// through echo cancellation, with the far-end reference supplied by a
/// paired `AecSink`. See [`wrap_with_echo_cancellation`].
struct AecSource {
    inner: Box<dyn uia_core::audio::AudioSource>,
    aec: Arc<Mutex<EchoCanceller>>,
}

#[async_trait::async_trait]
impl uia_core::audio::AudioSource for AecSource {
    fn format(&self) -> uia_core::audio::AudioFormat {
        self.inner.format()
    }

    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        let frame = self.inner.next_frame().await?;
        Some(self.aec.lock().unwrap().process_capture_frame(&frame))
    }
}

/// Wraps a playback `AudioSink` so every frame written to it is also fed to
/// the paired `AecSource` as the far-end reference, satisfying PLAN.md's "AEC
/// cannot work without it" requirement without either side needing to know
/// about the other directly.
struct AecSink {
    inner: Box<dyn uia_core::audio::AudioSink>,
    aec: Arc<Mutex<EchoCanceller>>,
}

#[async_trait::async_trait]
impl uia_core::audio::AudioSink for AecSink {
    fn format(&self) -> uia_core::audio::AudioFormat {
        self.inner.format()
    }

    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError> {
        self.aec.lock().unwrap().push_render_frame(frame);
        self.inner.write(frame).await
    }

    fn clear(&mut self) {
        self.inner.clear()
    }
}

/// Pairs a capture source and a playback sink through a shared echo
/// canceller: the sink's writes become the AEC's far-end reference, and the
/// source's frames come back already processed - which is what makes the
/// barge-in RMS detector in `uia_core::session::Session` (fed by
/// `source.next_frame()`) see the processed stream rather than the raw one.
///
/// When `cfg.enabled` is `false` this still wraps both, but the wrapping is
/// then a pass-through (see [`EchoCanceller::new`]), so callers never need to
/// special-case the disabled path themselves.
/// A wrapped `(AudioSource, AudioSink)` pair, returned by
/// [`wrap_with_echo_cancellation`].
type WrappedAudioPair = (
    Box<dyn uia_core::audio::AudioSource>,
    Box<dyn uia_core::audio::AudioSink>,
);

pub fn wrap_with_echo_cancellation(
    source: Box<dyn uia_core::audio::AudioSource>,
    sink: Box<dyn uia_core::audio::AudioSink>,
    cfg: EchoCancellerConfig,
) -> Result<WrappedAudioPair, AudioError> {
    let aec = Arc::new(Mutex::new(EchoCanceller::new(cfg)?));
    let wrapped_source = AecSource {
        inner: source,
        aec: aec.clone(),
    };
    let wrapped_sink = AecSink { inner: sink, aec };
    Ok((Box::new(wrapped_source), Box::new(wrapped_sink)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(
        freq_hz: f32,
        sample_rate_hz: f32,
        amplitude: f32,
        len: usize,
        start_sample: usize,
    ) -> Vec<i16> {
        (0..len)
            .map(|n| {
                let t = (start_sample + n) as f32 / sample_rate_hz;
                let s = amplitude * (2.0 * std::f32::consts::PI * freq_hz * t).sin();
                f32_to_i16(s)
            })
            .collect()
    }

    fn rms(frame: &[i16]) -> f32 {
        uia_core::audio::rms(frame)
    }

    /// Deterministic pseudo-noise (xorshift). Broadband on purpose: a pure
    /// sine's ~1ms period makes AEC3's delay estimator ambiguous (many
    /// delays alias to the same repeating waveform) and it never converges -
    /// confirmed empirically against the real library before writing this
    /// test. Real speech doesn't have that problem, and neither does noise.
    struct Xorshift(u32);
    impl Xorshift {
        fn next_f32(&mut self) -> f32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            (x as f32 / u32::MAX as f32) * 2.0 - 1.0
        }
    }
    fn noise_frame(rng: &mut Xorshift, amplitude: f32, len: usize) -> Vec<i16> {
        (0..len)
            .map(|_| f32_to_i16(amplitude * rng.next_f32()))
            .collect()
    }

    /// Feeds many frames of a played-back noise signal (the far-end/render
    /// reference) as-is into `push_render_frame`, and that same signal
    /// attenuated by a fixed gain (the acoustic echo) into
    /// `process_capture_frame` as the near-end capture, with nothing else in
    /// it. Runs enough frames (5s) for AEC3's adaptive filter to converge on
    /// the echo path, then asserts the final frame's output is attenuated
    /// well below the raw echo level - the failure PLAN.md describes:
    /// without this, the barge-in detector fires on the assistant's own
    /// voice.
    #[test]
    fn far_end_echo_is_attenuated_after_convergence() {
        let sample_rate_hz = 48_000;
        let mut aec = EchoCanceller::new(EchoCancellerConfig {
            enabled: true,
            sample_rate_hz,
        })
        .unwrap();

        let frame_len = (sample_rate_hz / 100) as usize; // 10ms
        let n_frames = 500; // 5s - generous headroom for AEC3 to converge
        let echo_gain = 0.6;
        let mut far_rng = Xorshift(12345);

        let mut last_output = Vec::new();
        let mut last_echo_only = Vec::new();

        for i in 0..n_frames {
            let far_end = noise_frame(&mut far_rng, 0.5, frame_len);
            let echo_only: Vec<i16> = far_end
                .iter()
                .map(|&f| f32_to_i16(i16_to_f32(f) * echo_gain))
                .collect();

            aec.push_render_frame(&far_end);
            let output = aec.process_capture_frame(&echo_only);

            if i == n_frames - 1 {
                last_output = output;
                last_echo_only = echo_only;
            }
        }

        assert!(
            !last_output.is_empty(),
            "processor produced no output for the final frame"
        );

        let echo_only_rms = rms(&last_echo_only);
        let output_rms = rms(&last_output);

        // Measured against the real library: a converged filter attenuates
        // this scenario to roughly 3% of the raw echo (~30dB). Asserting a
        // generous 30% ceiling keeps the test meaningful without being
        // brittle to minor library-version drift.
        assert!(
            output_rms < echo_only_rms * 0.3,
            "expected far-end echo to be attenuated after convergence: echo_only_rms={echo_only_rms}, output_rms={output_rms}"
        );
    }

    /// Near-end capture that is pure voice, with nothing playing on the
    /// far-end (an idle sink, or the moment just before the assistant starts
    /// speaking). Asserts the voice passes through close to unchanged -
    /// there is no echo to cancel, so the processor must not suppress
    /// genuine speech just because it is active.
    #[test]
    fn near_end_voice_survives_when_no_echo_is_present() {
        let sample_rate_hz = 48_000;
        let mut aec = EchoCanceller::new(EchoCancellerConfig {
            enabled: true,
            sample_rate_hz,
        })
        .unwrap();

        let frame_len = (sample_rate_hz / 100) as usize;
        let n_frames = 50;
        let voice_freq = 300.0;

        let mut last_output = Vec::new();
        let mut last_voice = Vec::new();

        for i in 0..n_frames {
            let start = i * frame_len;
            let silence = vec![0i16; frame_len];
            let voice = sine(voice_freq, sample_rate_hz as f32, 0.3, frame_len, start);

            aec.push_render_frame(&silence);
            let output = aec.process_capture_frame(&voice);

            if i == n_frames - 1 {
                last_output = output;
                last_voice = voice;
            }
        }

        assert!(
            !last_output.is_empty(),
            "processor produced no output for the final frame"
        );

        let voice_rms = rms(&last_voice);
        let output_rms = rms(&last_output);

        // Measured against the real library: ~5% reduction from the high
        // pass filter/other always-on processing, nowhere near suppressed.
        assert!(
            output_rms > voice_rms * 0.7,
            "expected near-end voice to survive when there is no echo to cancel: voice_rms={voice_rms}, output_rms={output_rms}"
        );
    }

    #[test]
    fn disabled_echo_canceller_is_pass_through() {
        let mut aec = EchoCanceller::new(EchoCancellerConfig {
            enabled: false,
            sample_rate_hz: 48_000,
        })
        .unwrap();
        aec.push_render_frame(&[1, 2, 3]);
        assert_eq!(aec.process_capture_frame(&[4, 5, 6]), vec![4, 5, 6]);
    }

    #[test]
    fn partial_frames_are_buffered_not_dropped() {
        let sample_rate_hz = 48_000;
        let mut aec = EchoCanceller::new(EchoCancellerConfig {
            enabled: true,
            sample_rate_hz,
        })
        .unwrap();
        let frame_len = (sample_rate_hz / 100) as usize;

        // Fewer samples than one 10ms frame: nothing should come out yet.
        let tiny = vec![0i16; frame_len / 4];
        aec.push_render_frame(&tiny);
        let out = aec.process_capture_frame(&tiny);
        assert!(out.is_empty(), "a partial frame must not be force-flushed");

        // Top it up past a full frame: now exactly one frame's worth is
        // ready (the rest stays buffered).
        let rest = vec![0i16; frame_len];
        aec.push_render_frame(&rest);
        let out = aec.process_capture_frame(&rest);
        assert_eq!(out.len(), frame_len);
    }

    /// Wires a `FixtureSource`/`VecSink` pair through
    /// `wrap_with_echo_cancellation` and confirms the source's frames come
    /// back already echo-cancelled - what makes
    /// `uia_core::session::Session`'s barge-in RMS check (which reads
    /// `source.next_frame()` directly, see `Session::pump_audio`) see the
    /// processed stream rather than the raw one, satisfying the stage's
    /// "confirm the barge-in RMS detector consumes the processed capture
    /// stream" step without touching `uia-core` at all.
    #[tokio::test]
    async fn wrapped_source_yields_echo_cancelled_frames_after_sink_writes() {
        use uia_core::audio::{AudioFormat, FixtureSource, VecSink};

        let sample_rate_hz = 48_000;
        let frame_len = (sample_rate_hz / 100) as usize;
        let format = AudioFormat {
            sample_rate_hz,
            channels: 1,
            encoding: uia_core::audio::Encoding::Pcm16Le,
        };

        let mut far_rng = Xorshift(555);
        let echo_gain = 0.6;

        // Enough capture frames (mirroring the far-end reference at reduced
        // gain, with no distinguishing voice) for the filter to converge -
        // same scenario as `far_end_echo_is_attenuated_after_convergence`,
        // just driven through the source/sink wrapper instead of the raw
        // `EchoCanceller` API.
        let n_frames = 500;
        let mut far_end_frames = Vec::with_capacity(n_frames);
        let mut capture_frames = Vec::with_capacity(n_frames);
        for _ in 0..n_frames {
            let far_end = noise_frame(&mut far_rng, 0.5, frame_len);
            let echo_only: Vec<i16> = far_end
                .iter()
                .map(|&f| f32_to_i16(i16_to_f32(f) * echo_gain))
                .collect();
            far_end_frames.push(far_end);
            capture_frames.push(echo_only);
        }
        let last_echo_only = capture_frames.last().cloned().unwrap();

        let source = Box::new(FixtureSource::new(format, capture_frames));
        let sink = Box::new(VecSink::new(format));

        let (mut wrapped_source, mut wrapped_sink) = wrap_with_echo_cancellation(
            source,
            sink,
            EchoCancellerConfig {
                enabled: true,
                sample_rate_hz,
            },
        )
        .unwrap();

        let mut last_yielded = Vec::new();
        for far_end in far_end_frames {
            // Real usage interleaves these per engine turn: the sink gets
            // written with what is about to play, and capture is pulled
            // alongside it - exactly what `Session::handle_event` (writes)
            // and `Session::pump_audio` (reads) do independently today.
            wrapped_sink.write(&far_end).await.unwrap();
            if let Some(frame) = wrapped_source.next_frame().await {
                last_yielded = frame;
            }
        }

        assert!(!last_yielded.is_empty(), "wrapped source produced no frame");
        let echo_only_rms = rms(&last_echo_only);
        let yielded_rms = rms(&last_yielded);
        assert!(
            yielded_rms < echo_only_rms * 0.3,
            "expected the wrapped source's frames to already be echo-cancelled: echo_only_rms={echo_only_rms}, yielded_rms={yielded_rms}"
        );
    }
}
