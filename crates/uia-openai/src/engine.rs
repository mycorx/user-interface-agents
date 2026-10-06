// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! `S2sEngine` for OpenAI Realtime over native WebRTC.
//!
//! Call sequence is the one S1 proved end to end (findings §3.3): build the peer
//! connection, register Opus, add a sendrecv audio track, open the `oai-events`
//! data channel, create the offer, **wait for ICE gathering to complete**, POST
//! the offer, apply the answer. Sending a half-gathered offer is the classic
//! hang.
//!
//! `webrtc` 0.20 is the sans-IO rewrite: no `APIBuilder`, no `on_message`
//! callbacks, no `gathering_complete_promise`. Events come from a
//! `PeerConnectionEventHandler`, and the data channel is **polled**.

use std::sync::Arc;

use async_trait::async_trait;
use rtc::media::Sample;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::shared::time::SystemInstant;
use tokio::sync::mpsc::Sender;
use uia_audio::codec::OpusCodec;
use uia_core::audio::AudioFormat;
use uia_core::engine::{
    EngineError, EngineEvent, EngineId, S2sEngine, SessionConfig, TextTurnSupport, ToolCallId,
};
use uia_core::tools::ToolResult;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState,
    RTCSessionDescription, Registry, register_default_interceptors,
};

use crate::creds::{mint_client_secret, resolve_api_key_from_env};
use crate::protocol::{
    HandshakeCapability, ToolNames, memory_item_events, parse_server_event, session_update,
    text_turn_event,
};

/// Pinned explicitly, never the floating `gpt-realtime` alias: an alias silently
/// changes the model under a provider A/B and makes S14 irreproducible.
pub const DEFAULT_MODEL: &str = "gpt-realtime-2.1-mini";

/// Both directions, taken from the live session object (findings §3.2). This is
/// the API's boundary format; the wire carries Opus at 48 kHz.
///
/// S1's findings guessed "the WebRTC stack owns that conversion" — it does not.
/// `webrtc`/`rtc` are protocol crates with no codec at all, so `uia-audio`'s
/// `OpusCodec` owns both the Opus coding and the 24↔48 kHz resampling.
const SAMPLE_RATE_HZ: u32 = 24_000;

/// Generous relative to the ~1s a healthy gather takes (measured 2.2s end to
/// end); it is a stall guard, not a tuning knob.
const ICE_GATHERING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const MIME_TYPE_OPUS: &str = "audio/opus";
const OPUS_PAYLOAD_TYPE: u8 = 111;

/// One Opus frame. `write_sample` turns this into the RTP timestamp increment,
/// so it must match what the codec actually encoded or playback drifts.
const FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(20);

/// Comfort-noise silence, not assistant speech. OpenAI's WebRTC track streams
/// silence RTP continuously the instant media flows — measured both before
/// and after `Ready`, and throughout otherwise-quiet stretches — so it must be
/// filtered before `EngineEvent::AudioChunk` reaches the session, or the
/// stream never satisfies "first event is Ready" and floods the sink with
/// nothing. An empty frame is not silence: it means nothing decoded, and
/// `on_track` already skips empty RTP payloads before decoding.
fn is_silence(pcm: &[i16]) -> bool {
    !pcm.is_empty() && pcm.iter().all(|&s| s == 0)
}

pub struct OpenAiEngine {
    api_key: String,
    model: String,
    http: reqwest::Client,
    conn: Option<Connection>,
    /// What the handshake said about typed turns (S2), cleared at the start
    /// of every `connect`.
    capability: HandshakeCapability,
}

struct Connection {
    pc: Arc<dyn PeerConnection>,
    dc: Arc<dyn DataChannel>,
    track: Arc<TrackLocalStaticSample>,
    /// The send-side half of the codec. Per connection, because an Opus encoder
    /// carries inter-frame state; the receive half lives in the `on_track` task.
    codec: OpusCodec,
    ssrc: u32,
    reader: tokio::task::JoinHandle<()>,
}

impl OpenAiEngine {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            http: reqwest::Client::new(),
            conn: None,
            capability: HandshakeCapability::new(),
        }
    }

    /// `OPENAI_API_KEY`, falling back to the gitignored key file.
    pub fn from_env() -> Result<Self, EngineError> {
        Ok(Self::new(
            resolve_api_key_from_env()?,
            DEFAULT_MODEL.to_string(),
        ))
    }

    async fn send_json(&self, v: serde_json::Value) -> Result<(), EngineError> {
        let conn = self.conn.as_ref().ok_or(EngineError::Closed)?;
        conn.dc
            .send_text(&v.to_string())
            .await
            .map_err(|e| EngineError::Transport(e.to_string()))?;
        Ok(())
    }
}

#[derive(Clone)]
struct Handler {
    gathered: tokio::sync::mpsc::Sender<()>,
    events: Sender<EngineEvent>,
}

#[async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }

    /// The assistant's voice arrives here, as RTP Opus on the remote track —
    /// never on the data channel, which carries metadata events only. Without
    /// this, `EngineEvent::AudioChunk` is never emitted and the sink stays
    /// silent no matter how healthy the session looks.
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let events = self.events.clone();
        // The decoder is created per track and owned solely by this task, so
        // its inter-frame state needs no lock and cannot be shared with the
        // send-side encoder.
        let mut codec = match OpusCodec::new() {
            Ok(c) => c,
            Err(e) => {
                let _ = events
                    .send(EngineEvent::Error(EngineError::Protocol(e.to_string())))
                    .await;
                return;
            }
        };
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                match event {
                    // RFC 7587: for Opus, one RTP packet carries exactly one
                    // Opus packet, so the payload needs no depacketiser.
                    TrackRemoteEvent::OnRtpPacket(pkt) => {
                        if pkt.payload.is_empty() {
                            continue;
                        }
                        match codec.decode(&pkt.payload) {
                            Ok(pcm) => {
                                if is_silence(&pcm) {
                                    continue;
                                }
                                if events.send(EngineEvent::AudioChunk(pcm)).await.is_err() {
                                    break;
                                }
                            }
                            // A single undecodable packet is packet loss, not a
                            // broken session. Surfacing it would classify a
                            // healthy call as failed and reconnect it — the same
                            // trap `response_cancel_not_active` set in S9.
                            Err(_) => continue,
                        }
                    }
                    TrackRemoteEvent::OnEnded => break,
                    _ => {}
                }
            }
        });
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        // A dropped peer connection is transient: the session reconnects with
        // backoff. Auth never lands here — it fails earlier, at the mint.
        match state {
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected => {
                let _ = self
                    .events
                    .send(EngineEvent::Error(EngineError::Transport(format!(
                        "peer connection {state}"
                    ))))
                    .await;
            }
            RTCPeerConnectionState::Closed => {
                let _ = self.events.send(EngineEvent::Closed).await;
            }
            _ => {}
        }
    }
}

#[async_trait]
impl S2sEngine for OpenAiEngine {
    fn id(&self) -> EngineId {
        EngineId::OpenAi
    }

    fn input_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(SAMPLE_RATE_HZ)
    }

    fn output_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(SAMPLE_RATE_HZ)
    }

    async fn connect(
        &mut self,
        cfg: &SessionConfig,
        tx: Sender<EngineEvent>,
    ) -> Result<(), EngineError> {
        // This connection has been told nothing yet, and the last one's
        // answer is not evidence about this one.
        self.capability.reset();

        // 1. Ephemeral secret. The ONLY use of the long-lived key.
        let ephemeral = mint_client_secret(&self.http, &self.api_key, &self.model).await?;

        // 2. Peer connection with Opus registered.
        let mut media_engine = MediaEngine::default();
        let audio_codec = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            payload_type: OPUS_PAYLOAD_TYPE,
        };
        media_engine
            .register_codec(audio_codec.clone(), RtpCodecKind::Audio)
            .map_err(|e| EngineError::Transport(e.to_string()))?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| EngineError::Transport(e.to_string()))?;

        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_string()],
                ..Default::default()
            }])
            .build();

        let (gathered_tx, mut gathered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let pc = PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler {
                gathered: gathered_tx,
                events: tx.clone(),
            }))
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await
            .map_err(|e| EngineError::Transport(e.to_string()))?;
        // `build()` returns `impl PeerConnection`; erase it so it can be stored.
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);

        // 3. Sendrecv Opus track. WebRTC carries Opus, never raw PCM.
        let ssrc = rand::random::<u32>();
        let track: Arc<TrackLocalStaticSample> = Arc::new(
            TrackLocalStaticSample::new(MediaStreamTrack::new(
                "uia-stream".to_owned(),
                "uia-audio".to_owned(),
                "uia-audio".to_owned(),
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: audio_codec.rtp_codec.clone(),
                    ..Default::default()
                }],
            ))
            .map_err(|e| EngineError::Transport(e.to_string()))?,
        );
        pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|e| EngineError::Transport(e.to_string()))?;

        // 4. Events data channel.
        let dc = pc
            .create_data_channel("oai-events", None)
            .await
            .map_err(|e| EngineError::Transport(e.to_string()))?;

        // 5. Offer, FULL ICE gather, then the SDP exchange.
        let offer = pc
            .create_offer(None)
            .await
            .map_err(|e| EngineError::Transport(e.to_string()))?;
        pc.set_local_description(offer)
            .await
            .map_err(|e| EngineError::Transport(e.to_string()))?;
        // Bounded: an unbounded await here hangs the assistant forever if the
        // notification never arrives. Sending a half-gathered offer is the
        // classic hang, so we wait — but not indefinitely.
        let _ = tokio::time::timeout(ICE_GATHERING_TIMEOUT, gathered_rx.recv()).await;
        let offer_sdp = pc
            .local_description()
            .await
            .ok_or_else(|| EngineError::Transport("no local description".into()))?
            .unmarshal()
            .map_err(|e| EngineError::Protocol(e.to_string()))?
            .marshal();

        let resp = self
            .http
            .post(format!(
                "https://api.openai.com/v1/realtime/calls?model={}",
                self.model
            ))
            .bearer_auth(&ephemeral)
            .header("Content-Type", "application/sdp")
            .body(offer_sdp)
            .send()
            .await
            .map_err(|e| EngineError::Transport(e.to_string()))?;
        let status = resp.status();
        let answer_sdp = resp
            .text()
            .await
            .map_err(|e| EngineError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(match status {
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                    EngineError::Auth(format!("SDP exchange rejected ({status})"))
                }
                _ => EngineError::Transport(format!("SDP exchange failed ({status})")),
            });
        }
        pc.set_remote_description(
            RTCSessionDescription::answer(answer_sdp)
                .map_err(|e| EngineError::Protocol(e.to_string()))?,
        )
        .await
        .map_err(|e| EngineError::Transport(e.to_string()))?;

        // 6. Read task: poll the data channel, translate, forward.
        let update = session_update(cfg);
        // Prior exchanges, replayed as conversation items once the channel is
        // up. Built here, alongside `update`, because `cfg` does not outlive
        // this call while the read task does.
        let history = memory_item_events(cfg);
        // Tools are declared under sanitised names; map them back so the
        // executor sees the namespaced name it is keyed by.
        let tool_names = ToolNames::new(&cfg.tools);
        let dc_read: Arc<dyn DataChannel> = dc.clone();
        let dc_write: Arc<dyn DataChannel> = dc.clone();
        let capability = self.capability.clone();
        let reader = tokio::spawn(async move {
            while let Some(ev) = dc_read.poll().await {
                match ev {
                    DataChannelEvent::OnOpen => {
                        // Tools and options are declared once the channel opens.
                        let _ = dc_write.send_text(&update.to_string()).await;
                        // Then what was said last time, oldest first. After the
                        // update so the items land in a session that already
                        // knows its instructions, and with no `response.create`
                        // after them: replaying history must not make the
                        // assistant start talking about it.
                        for item in &history {
                            let _ = dc_write.send_text(&item.to_string()).await;
                        }
                    }
                    DataChannelEvent::OnMessage(msg) => {
                        let text = String::from_utf8_lossy(&msg.data);
                        if std::env::var("UIA_DUMP_EVENTS").is_ok() {
                            eprintln!("RAW {text}");
                        }
                        // Read for capability BEFORE translating: the same
                        // `session.created` that reports `Ready` is the one
                        // carrying the modalities (FD4), and reading it for
                        // capability must never cost the session an event.
                        capability.observe(&text);
                        match parse_server_event(&text) {
                            Ok(events) => {
                                for e in events {
                                    let e = match e {
                                        EngineEvent::ToolCall { id, name, args } => {
                                            EngineEvent::ToolCall {
                                                id,
                                                name: tool_names.restore(&name),
                                                args,
                                            }
                                        }
                                        other => other,
                                    };
                                    if tx.send(e).await.is_err() {
                                        return; // receiver gone
                                    }
                                }
                            }
                            // A malformed event is reported, never fatal.
                            Err(e) => {
                                if tx.send(EngineEvent::Error(e)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    DataChannelEvent::OnClose => {
                        let _ = tx.send(EngineEvent::Closed).await;
                        return;
                    }
                    _ => {}
                }
            }
            let _ = tx.send(EngineEvent::Closed).await;
        });

        self.conn = Some(Connection {
            pc,
            dc,
            track,
            codec: OpusCodec::new().map_err(|e| EngineError::Protocol(e.to_string()))?,
            ssrc,
            reader,
        });
        Ok(())
    }

    async fn send_audio(&mut self, frame: &[i16]) -> Result<(), EngineError> {
        let conn = self.conn.as_mut().ok_or(EngineError::Closed)?;

        // `TrackLocalStaticSample` takes already-encoded samples and paces them
        // by `duration`, so every packet must be exactly one 20 ms frame. The
        // codec buffers whatever the device handed us and gives back only whole
        // frames; a short capture buffer yields no packets, not a short one.
        let packets = conn
            .codec
            .encode_stream(frame)
            .map_err(|e| EngineError::Protocol(e.to_string()))?;

        for payload in packets {
            let sample = Sample {
                data: bytes::Bytes::from(payload),
                timestamp: SystemInstant::now(),
                duration: FRAME_DURATION,
                packet_timestamp: 0,
                // Both counters describe packets lost *before* this sample.
                // We are the source, so nothing was lost on the way in.
                prev_dropped_packets: 0,
                prev_padding_packets: 0,
            };
            conn.track
                .write_sample(conn.ssrc, OPUS_PAYLOAD_TYPE, &sample, &[])
                .await
                .map_err(|e| EngineError::Transport(e.to_string()))?;
        }
        Ok(())
    }

    async fn send_tool_result(
        &mut self,
        id: ToolCallId,
        result: ToolResult,
    ) -> Result<(), EngineError> {
        // `call_id` is what the output must reference — never `item_id`.
        self.send_json(serde_json::json!({
            "type": "conversation.item.create",
            "item": {"type": "function_call_output", "call_id": id, "output": result.content}
        }))
        .await?;
        // The model does not speak the result until a response is requested.
        self.send_json(serde_json::json!({"type": "response.create"}))
            .await
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
        // Secondary to the local sink clear, which already happened and is
        // authoritative. This only stops the provider generating more.
        self.send_json(serde_json::json!({"type": "response.cancel"}))
            .await
    }

    async fn close(&mut self) -> Result<(), EngineError> {
        if let Some(conn) = self.conn.take() {
            conn.reader.abort();
            conn.pc
                .close()
                .await
                .map_err(|e| EngineError::Transport(e.to_string()))?;
        }
        Ok(())
    }

    async fn send_text(&mut self, text: &str) -> Result<(), EngineError> {
        self.send_json(text_turn_event(text)).await?;
        // Same as `send_tool_result`: creating the item alone does not make
        // the model speak to it.
        self.send_json(serde_json::json!({"type": "response.create"}))
            .await
    }

    /// The modalities this session echoed at handshake (FD4), falling back
    /// to the measured baseline when the handshake made no claim.
    ///
    /// `Supported` is the right fallback here and nowhere else: OpenAI
    /// Realtime is the one engine where typed turns are proven end to end
    /// (S0) - `send_text` above sends the GA `conversation.item.create` +
    /// `response.create` pair and the model answers - so an uninformative or
    /// absent handshake leaves a measured fact standing rather than talking
    /// it down to a guess.
    fn text_turn_support(&self) -> TextTurnSupport {
        self.capability.get_or(TextTurnSupport::Supported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_turns_are_reported_supported() {
        // OpenAI Realtime is the one path proven end to end (PLAN.md S0), and
        // it is the baseline S2's handshake-derived detection is measured
        // against — so this is asserted, not assumed.
        let e = OpenAiEngine::new("k".into(), DEFAULT_MODEL.into());
        assert_eq!(e.text_turn_support(), TextTurnSupport::Supported);
        assert!(e.text_turn_support().is_usable());
        assert_eq!(e.text_turn_support().reason(), None);
    }

    #[test]
    fn a_handshake_that_excludes_text_takes_the_control_away() {
        // The static `Supported` above is a fallback, not a hardcode: a
        // session that positively reports no text channel overrides it, and
        // the reason it carries is what the HUD shows (FD6).
        let e = OpenAiEngine::new("k".into(), DEFAULT_MODEL.into());
        e.capability
            .observe(r#"{"type":"session.created","session":{"modalities":["audio"]}}"#);
        let got = e.text_turn_support();
        assert!(!got.is_usable(), "got {got:?}");
        assert!(got.reason().is_some_and(|r| r.contains("speak instead")));
    }

    #[test]
    fn an_audio_only_output_handshake_leaves_the_proven_answer_alone() {
        // A GA `gpt-realtime` session is audio-OUT by default and still takes
        // typed input - the exact path S0 proved. This is the regression the
        // parser's field-by-field reading exists to prevent (FD5).
        let e = OpenAiEngine::new("k".into(), DEFAULT_MODEL.into());
        e.capability
            .observe(r#"{"type":"session.created","session":{"output_modalities":["audio"]}}"#);
        assert_eq!(e.text_turn_support(), TextTurnSupport::Supported);
    }

    #[test]
    fn both_directions_are_mono_pcm16_at_24_khz() {
        // Measured from the live session object in S1 (findings §3.2), not
        // assumed. A wrong constant here is a silent audio-quality bug: it is
        // what `Session` resamples the microphone to.
        let e = OpenAiEngine::new("k".into(), DEFAULT_MODEL.into());
        assert_eq!(e.input_format(), AudioFormat::mono_pcm16(24_000));
        assert_eq!(e.output_format(), AudioFormat::mono_pcm16(24_000));
        assert_eq!(e.id(), EngineId::OpenAi);
    }

    #[test]
    fn comfort_noise_silence_is_detected() {
        // OpenAI's WebRTC track streams continuous silence RTP the instant
        // media flows, both before and after `Ready` (live-measured 2026-08-19:
        // silence arrives ~2.17s in, `Ready` not until ~2.53s, silence continues
        // long after). Emitting every one as `AudioChunk` violates "first event
        // is Ready" and floods the stream with content-free audio. All-zero PCM
        // is comfort noise, not assistant speech, and must not be emitted.
        assert!(is_silence(&[0i16; 480]));
        assert!(!is_silence(&[0, 0, 5, 0]));
        assert!(!is_silence(&[]));
    }

    #[test]
    fn the_default_model_is_pinned_not_a_floating_alias() {
        // A floating alias silently changes the model under S14's A/B.
        assert_eq!(DEFAULT_MODEL, "gpt-realtime-2.1-mini");
        assert!(
            DEFAULT_MODEL.ends_with("-mini"),
            "mini is ~3.2x cheaper on audio"
        );
    }

    #[tokio::test]
    async fn sending_before_connecting_is_closed_not_a_panic() {
        let mut e = OpenAiEngine::new("k".into(), DEFAULT_MODEL.into());
        assert!(matches!(e.interrupt().await, Err(EngineError::Closed)));
        // `send_audio` used to return a hardcoded "no Opus encoder" error. Now
        // that it really encodes, the only reason it can fail before connect is
        // that there is no track to write to.
        assert!(matches!(
            e.send_audio(&[0i16; 480]).await,
            Err(EngineError::Closed)
        ));
        assert!(matches!(
            e.send_tool_result("call_1".into(), ToolResult::ok("{}"))
                .await,
            Err(EngineError::Closed)
        ));
        assert!(matches!(e.send_text("hi").await, Err(EngineError::Closed)));
        // close() on a never-connected engine is a no-op, not an error.
        assert!(e.close().await.is_ok());
    }
}
