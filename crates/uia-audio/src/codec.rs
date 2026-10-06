// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Opus codec for the WebRTC audio path, in both directions.
//!
//! WebRTC carries Opus; the `webrtc`/`rtc` crates are protocol implementations
//! and ship no codec at all. `TrackLocalStaticSample` takes already-encoded
//! samples, and the assistant's voice arrives as RTP Opus on the remote track —
//! so this is the one place PCM becomes Opus and back.
//!
//! Pure Rust (`opus-rs`), deliberately: see PLAN.md's frozen decision. No cmake,
//! no C cross-compilation toolchain on the path to iOS or Android.
//!
//! **The codec runs at 48 kHz, not at our 24 kHz boundary rate**, and that is
//! load-bearing — see [`WIRE_RATE_HZ`].

use opus_rs::{Application, OpusDecoder, OpusEncoder};
use thiserror::Error;
use uia_core::audio::{Resampler, downmix_to_mono, f32_to_i16, i16_to_f32};

/// The API boundary format from the live session object (findings §3.2): what
/// `Session` resamples the microphone to, and what the sink plays back.
pub const BOUNDARY_RATE_HZ: u32 = 24_000;

/// The rate Opus itself runs at, in both directions.
///
/// 48 kHz is the RTP clock rate for Opus and what the remote track negotiated
/// (`audio/opus 48000/2`), so this is the WebRTC-native choice anyway. But it is
/// also **required**, because `opus-rs` 0.1.29's 24 kHz path is broken: a 24 kHz
/// encoder selects Hybrid-SWB, which round-trips a 440 Hz sine at ~1 dB SNR with
/// *more* output energy than input — it injects noise rather than losing detail.
/// Measured 2026-08-19 across every bitrate from 16 to 64 kbps. Upstream's own
/// test matrix covers only 16 kHz (SILK, 31–33 dB) and 48 kHz (Hybrid-FB,
/// 30–40 dB), so the 24 kHz hole is untested there and all 100+ of their tests
/// pass regardless.
///
/// Resampling 24 kHz up to 48 kHz costs no fidelity that matters: OpenAI's
/// session declares 24 kHz PCM in *and* out, so the model neither consumes nor
/// produces anything above 12 kHz. The upsample is a transport requirement, not
/// a quality one.
pub const WIRE_RATE_HZ: u32 = 48_000;

/// 20 ms at the boundary rate — the frame `send_audio` accepts.
pub const FRAME_SAMPLES: usize = (BOUNDARY_RATE_HZ as usize / 1000) * 20;

/// 20 ms at the wire rate — the frame Opus encodes.
const WIRE_FRAME_SAMPLES: usize = (WIRE_RATE_HZ as usize / 1000) * 20;

/// Constant bitrate at 32 kbps, and both halves of that are deliberate.
///
/// 32 kbps mono is comfortably transparent for speech, and CBR gives a
/// predictable packet size for a real-time voice path. But CBR is also
/// *required* here: `opus-rs` picks its internal mode from an equivalent-rate
/// estimate that treats VBR as a higher rate, and at 48 kHz VBR crosses the
/// threshold into **CELT-only** — which is broken in 0.1.29 the same way the
/// 24 kHz path is (2.1 dB round-trip SNR, measured 2026-08-19). CBR keeps the
/// encoder in Hybrid-FB, the configuration upstream actually tests, at 39 dB.
const BITRATE_BPS: i32 = 32_000;

/// RFC 6716 §3.1 caps a single Opus packet at 1276 bytes of data.
const MAX_PACKET_BYTES: usize = 1276;

/// 120 ms at the wire rate — the longest packet RFC 6716 allows. We only ever
/// *send* 20 ms, but a peer may send more, so the decode buffer fits the max.
const MAX_DECODE_SAMPLES: usize = (WIRE_RATE_HZ as usize / 1000) * 120;

#[derive(Debug, Error)]
pub enum CodecError {
    /// `opus-rs` reports failures as `&'static str`, not a typed error.
    #[error("opus: {0}")]
    Opus(&'static str),
    #[error("expected {FRAME_SAMPLES} samples (20 ms mono at 24 kHz), got {0}")]
    FrameSize(usize),
    #[error("resample: {0}")]
    Resample(uia_core::audio::AudioError),
}

/// Opus in both directions for one WebRTC session.
///
/// Stateful and single-session by construction: an Opus encoder carries
/// inter-frame state, so sharing one across sessions would leak audio context
/// between them. Create one per connection.
pub struct OpusCodec {
    encoder: OpusEncoder,
    /// Two decoders, because `opus-rs` **rejects** a packet whose channel count
    /// differs from the decoder's rather than downmixing the way libopus does
    /// (`decode` → "Channel count mismatch between packet and decoder"). The
    /// remote track negotiated `audio/opus 48000/2`, so a stereo packet is a
    /// live possibility and dropping it would be silence, not an error anyone
    /// hears. Each decoder keeps its own inter-frame state, so both are kept
    /// side by side rather than rebuilt per packet.
    decoder_mono: OpusDecoder,
    decoder_stereo: OpusDecoder,
    upsample: Resampler,
    downsample: Resampler,
    packet_buf: Vec<u8>,
    pcm_buf: Vec<f32>,
    /// Capture samples not yet forming a whole 20 ms frame, held for the next
    /// `encode_stream` call.
    frame_buf: Vec<i16>,
}

impl OpusCodec {
    pub fn new() -> Result<Self, CodecError> {
        let rate = WIRE_RATE_HZ as i32;
        // `Voip` rather than `Audio`: this is a speech path, and it biases Opus
        // toward intelligibility over musical fidelity.
        let mut encoder = OpusEncoder::new(rate, 1, Application::Voip).map_err(CodecError::Opus)?;
        encoder.bitrate_bps = BITRATE_BPS;
        encoder.use_cbr = true;
        Ok(Self {
            encoder,
            decoder_mono: OpusDecoder::new(rate, 1).map_err(CodecError::Opus)?,
            decoder_stereo: OpusDecoder::new(rate, 2).map_err(CodecError::Opus)?,
            upsample: Resampler::new(BOUNDARY_RATE_HZ, WIRE_RATE_HZ, FRAME_SAMPLES)
                .map_err(CodecError::Resample)?,
            downsample: Resampler::new(WIRE_RATE_HZ, BOUNDARY_RATE_HZ, WIRE_FRAME_SAMPLES)
                .map_err(CodecError::Resample)?,
            packet_buf: vec![0; MAX_PACKET_BYTES],
            pcm_buf: vec![0.0; MAX_DECODE_SAMPLES * 2],
            frame_buf: Vec::with_capacity(FRAME_SAMPLES * 2),
        })
    }

    /// Encode exactly one 20 ms mono frame at the boundary rate.
    ///
    /// Rejects any other length rather than padding or splitting. Wrong framing
    /// does not fail at the transport — it just sounds wrong — so the check
    /// belongs here, where it is still loud.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, CodecError> {
        if pcm.len() != FRAME_SAMPLES {
            return Err(CodecError::FrameSize(pcm.len()));
        }
        let wire = self.upsample.process(pcm).map_err(CodecError::Resample)?;
        let floats = i16_to_f32(&wire);
        let n = self
            .encoder
            .encode(&floats, floats.len(), &mut self.packet_buf)
            .map_err(CodecError::Opus)?;
        Ok(self.packet_buf[..n].to_vec())
    }

    /// Encode an arbitrary run of boundary-rate PCM, emitting one packet per
    /// whole 20 ms frame and carrying any remainder to the next call.
    ///
    /// This is what the engine calls. Capture hardware hands us whatever buffer
    /// size the device picked — never 20 ms — so something has to re-frame, and
    /// doing it here means the device's buffer size cannot change the bytes on
    /// the wire.
    pub fn encode_stream(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>, CodecError> {
        self.frame_buf.extend_from_slice(pcm);
        let mut packets = Vec::with_capacity(self.frame_buf.len() / FRAME_SAMPLES);
        while self.frame_buf.len() >= FRAME_SAMPLES {
            let frame: Vec<i16> = self.frame_buf.drain(..FRAME_SAMPLES).collect();
            packets.push(self.encode(&frame)?);
        }
        Ok(packets)
    }

    /// Decode one Opus packet to mono PCM at the boundary rate.
    ///
    /// A stereo packet is downmixed after decoding, since the codec itself will
    /// not do it, and the result is resampled back down to [`BOUNDARY_RATE_HZ`].
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, CodecError> {
        let stereo = packet.first().ok_or(CodecError::Opus("empty packet"))? & 0x04 != 0;
        let channels = if stereo { 2 } else { 1 };
        let decoder = if stereo {
            &mut self.decoder_stereo
        } else {
            &mut self.decoder_mono
        };

        let out = &mut self.pcm_buf[..MAX_DECODE_SAMPLES * channels];
        let samples = decoder
            .decode(packet, MAX_DECODE_SAMPLES, out)
            .map_err(CodecError::Opus)?;

        let pcm = f32_to_i16(&out[..samples * channels]);
        let mono = downmix_to_mono(&pcm, channels as u16);
        self.downsample.process(&mono).map_err(CodecError::Resample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 440 Hz tone is a better fixture than silence: silence encodes to a
    /// near-empty packet and would hide a codec that is doing nothing at all.
    fn tone(rate: u32, samples: usize, channels: usize, phase0: usize) -> Vec<i16> {
        (0..samples)
            .flat_map(|n| {
                let t = (n + phase0) as f32 / rate as f32;
                let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 8_000.0) as i16;
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    /// Signal-to-noise ratio in dB at the delay that best aligns the two
    /// signals — the method `opus-rs`'s own quality suite uses. A codec is
    /// allowed to delay audio; it is not allowed to change it.
    ///
    /// This must not be weakened to an energy comparison. The 24 kHz path this
    /// codec deliberately avoids returns *more* energy than it was given, so an
    /// energy check passes on pure noise.
    fn snr_db(input: &[i16], output: &[i16], max_delay: usize) -> f64 {
        let (start, end) = (input.len() / 4, input.len() * 3 / 4);
        let mut best = (f64::NEG_INFINITY, 0usize);
        for d in 0..=max_delay {
            let corr: f64 = (start..end)
                .filter(|i| i + d < output.len())
                .map(|i| input[i] as f64 * output[i + d] as f64)
                .sum();
            if corr > best.0 {
                best = (corr, d);
            }
        }
        let d = best.1;
        let (mut sig, mut noise) = (0.0f64, 0.0f64);
        for i in start..end {
            if i + d < output.len() {
                let s = input[i] as f64;
                sig += s * s;
                noise += (s - output[i + d] as f64).powi(2);
            }
        }
        10.0 * (sig / noise.max(1e-12)).log10()
    }

    fn round_trip(codec: &mut OpusCodec, frames: usize) -> (Vec<i16>, Vec<i16>) {
        let input = tone(BOUNDARY_RATE_HZ, FRAME_SAMPLES * frames, 1, 0);
        let mut output = Vec::with_capacity(input.len());
        for f in 0..frames {
            let chunk = &input[f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES];
            let packet = codec.encode(chunk).expect("encode");
            output.extend_from_slice(&codec.decode(&packet).expect("decode"));
        }
        (input, output)
    }

    #[test]
    fn encoding_one_20ms_frame_yields_a_nonempty_packet() {
        let mut codec = OpusCodec::new().expect("codec");
        let packet = codec
            .encode(&tone(BOUNDARY_RATE_HZ, FRAME_SAMPLES, 1, 0))
            .expect("encode");
        assert!(!packet.is_empty(), "a 20 ms frame must encode to something");
        assert!(
            packet.len() < FRAME_SAMPLES * 2,
            "encoded {} bytes, no smaller than the raw PCM it replaced",
            packet.len()
        );
    }

    #[test]
    fn encoding_rejects_a_frame_that_is_not_20ms() {
        // Wrong framing is the quiet failure mode for real-time audio: it does
        // not error at the transport, it just sounds wrong. Reject it here.
        let mut codec = OpusCodec::new().expect("codec");
        assert!(codec.encode(&tone(BOUNDARY_RATE_HZ, 160, 1, 0)).is_err());
        assert!(
            codec
                .encode(&tone(BOUNDARY_RATE_HZ, FRAME_SAMPLES + 1, 1, 0))
                .is_err()
        );
        assert!(codec.encode(&[]).is_err());
    }

    #[test]
    fn a_round_trip_preserves_the_frame_length() {
        let mut codec = OpusCodec::new().expect("codec");
        let packet = codec
            .encode(&tone(BOUNDARY_RATE_HZ, FRAME_SAMPLES, 1, 0))
            .expect("encode");
        let pcm = codec.decode(&packet).expect("decode");
        assert_eq!(
            pcm.len(),
            FRAME_SAMPLES,
            "480 samples in must be 480 samples out, or playback drifts"
        );
    }

    #[test]
    fn a_round_trip_reproduces_the_signal() {
        let mut codec = OpusCodec::new().expect("codec");
        let (input, output) = round_trip(&mut codec, 30);
        let snr = snr_db(&input, &output, FRAME_SAMPLES);
        assert!(
            snr >= 10.0,
            "round-trip SNR {snr:.2} dB is below the 10 dB floor — the codec is \
             not reproducing the signal (upstream's own floor for a working \
             configuration is 11 dB)"
        );
    }

    #[test]
    fn the_encoder_stays_on_the_hybrid_fullband_path() {
        // Guards the two settings this codec depends on for correctness rather
        // than taste: 48 kHz (not the boundary rate) and CBR (not VBR). Both of
        // opus-rs 0.1.29's other modes are broken at ~1-2 dB round-trip SNR, and
        // both failures are silent — audio still flows, it is just noise. So
        // assert the mode structurally, on the wire, where it cannot drift.
        //
        // RFC 6716 §3.1 TOC config: 0-11 SILK, 12-15 Hybrid, 16-31 CELT-only.
        let mut codec = OpusCodec::new().expect("codec");
        let packet = codec
            .encode(&tone(BOUNDARY_RATE_HZ, FRAME_SAMPLES, 1, 0))
            .expect("encode");
        let config = packet[0] >> 3;
        assert!(
            (12..=15).contains(&config),
            "TOC config {config} is not Hybrid — the encoder has drifted onto \
             one of opus-rs's broken paths (SILK below 12, CELT-only above 15)"
        );
    }

    #[test]
    fn a_stereo_packet_decodes_to_mono() {
        // opus-rs refuses a channel-count mismatch outright rather than
        // downmixing the way libopus does, and OpenAI's remote track
        // negotiated 48000/2 — so the receive path must handle a stereo
        // packet arriving at a mono sink.
        let mut stereo =
            OpusEncoder::new(WIRE_RATE_HZ as i32, 2, Application::Voip).expect("stereo encoder");
        let frame = WIRE_FRAME_SAMPLES;
        let mut buf = vec![0u8; MAX_PACKET_BYTES];
        let n = stereo
            .encode(
                &i16_to_f32(&tone(WIRE_RATE_HZ, frame, 2, 0)),
                frame,
                &mut buf,
            )
            .expect("stereo encode");
        let packet = &buf[..n];
        assert_eq!(packet[0] & 0x04, 0x04, "fixture must be a stereo packet");

        let mut codec = OpusCodec::new().expect("codec");
        let pcm = codec.decode(packet).expect("decode stereo");
        assert_eq!(
            pcm.len(),
            FRAME_SAMPLES,
            "a stereo 48 kHz packet must yield one mono frame at the boundary rate"
        );
    }

    #[test]
    fn streaming_encode_buffers_until_a_whole_frame_is_available() {
        // Capture hardware delivers whatever buffer size the device chose --
        // never 20 ms frames. The engine must not hand a partial frame to the
        // encoder, nor drop the remainder.
        let mut codec = OpusCodec::new().expect("codec");
        assert!(
            codec
                .encode_stream(&tone(BOUNDARY_RATE_HZ, 100, 1, 0))
                .expect("encode")
                .is_empty(),
            "100 samples is a quarter of a frame; nothing may go out yet"
        );
        let packets = codec
            .encode_stream(&tone(BOUNDARY_RATE_HZ, 380, 1, 100))
            .expect("encode");
        assert_eq!(packets.len(), 1, "100 + 380 completes exactly one frame");
    }

    #[test]
    fn streaming_encode_emits_every_whole_frame_in_a_large_buffer() {
        let mut codec = OpusCodec::new().expect("codec");
        let packets = codec
            .encode_stream(&tone(BOUNDARY_RATE_HZ, FRAME_SAMPLES * 3 + 40, 1, 0))
            .expect("encode");
        assert_eq!(packets.len(), 3, "three whole frames, 40 samples carried");
        // The carried 40 must still be there: 440 more completes a fourth.
        let more = codec
            .encode_stream(&tone(BOUNDARY_RATE_HZ, 440, 1, 0))
            .expect("encode");
        assert_eq!(more.len(), 1, "the carried remainder was dropped");
    }

    #[test]
    fn streaming_encode_is_independent_of_how_input_is_chunked() {
        // The device's buffer size must not change a single byte on the wire.
        let signal = tone(BOUNDARY_RATE_HZ, FRAME_SAMPLES * 10, 1, 0);

        let mut whole = OpusCodec::new().expect("codec");
        let expected = whole.encode_stream(&signal).expect("encode");

        let mut chunked = OpusCodec::new().expect("codec");
        let mut got = Vec::new();
        for piece in signal.chunks(157) {
            got.extend(chunked.encode_stream(piece).expect("encode"));
        }

        assert_eq!(got.len(), expected.len(), "different packet counts");
        assert_eq!(
            got, expected,
            "ragged 157-sample chunks changed the wire bytes"
        );
    }

    #[test]
    fn decoding_rejects_an_empty_packet() {
        let mut codec = OpusCodec::new().expect("codec");
        assert!(codec.decode(&[]).is_err());
    }
}
