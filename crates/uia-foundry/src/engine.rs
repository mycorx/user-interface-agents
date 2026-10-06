// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! `S2sEngine` for Azure AI Foundry's GPT Realtime API, over native WebRTC.
//!
//! Sourced from Microsoft's own GA documentation (Realtime via WebRTC, and
//! the Preview-to-GA migration guide) rather than guessed, and then confirmed
//! against a live `services.ai.azure.com` resource
//! (`foundry_satisfies_the_engine_contract`, `cargo test -p uia-foundry --
//! --ignored`): the ephemeral-secret mint is `POST
//! {endpoint}/openai/v1/realtime/client_secrets` with an `api-key` header
//! (not `Authorization: Bearer` — that's reserved for Entra ID auth, which
//! this engine does not implement), and the SDP exchange is `POST
//! {endpoint}/openai/v1/realtime/calls` with `Authorization: Bearer
//! {ephemeral}`. Both differ from OpenAI's own `api.openai.com` URLs and
//! long-lived-key auth, but the event schema and the ephemeral-secret
//! response shape are byte-for-byte the same GA protocol — Microsoft's
//! migration guide says as much ("GA protocol and message format are only
//! supported in the SDKs provided by OpenAI") and the live run proved it: no
//! translation layer was needed between `uia_openai::protocol`'s event
//! parsing and what a real Foundry resource actually sent. Everything past
//! the connection itself reuses that module and
//! `uia_openai::creds::parse_client_secret` rather than re-deriving an
//! identical state machine.
//!
//! Call sequence mirrors `uia_openai::engine::OpenAiEngine::connect`
//! exactly (same reasoning: build the peer connection, register Opus, add a
//! sendrecv audio track, open the events data channel, create the offer,
//! wait for full ICE gathering, POST the offer, apply the answer) — only the
//! two URLs and the mint's auth header differ.

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
use uia_openai::creds::parse_client_secret;
use uia_openai::protocol::{
    HandshakeCapability, ToolNames, memory_item_events, parse_server_event, session_update,
    text_turn_event,
};
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

/// Both directions, same as OpenAI's own GA `gpt-realtime*` family — this is
/// the API's boundary format, not a Foundry-specific choice; the wire itself
/// carries Opus at 48 kHz, same as `uia_openai::engine`.
const SAMPLE_RATE_HZ: u32 = 24_000;

/// The deployment name verified live end to end
/// (`foundry_satisfies_the_engine_contract`) — mirrors
/// `uia_openai::engine::DEFAULT_MODEL`'s role as a pinned, non-floating
/// default for this crate's own convenience constructors and test harnesses.
/// A caller with a different deployment provisioned passes its own name to
/// [`FoundryEngine::new`] directly; this is not read by `connect()`.
pub const DEFAULT_DEPLOYMENT: &str = "gpt-realtime-2.1";

/// Same stall guard as `uia_openai::engine::ICE_GATHERING_TIMEOUT` — not
/// re-exported from that crate, so restated here rather than adding a public
/// constant to a crate this one otherwise only reads the protocol/creds
/// helpers from.
const ICE_GATHERING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const MIME_TYPE_OPUS: &str = "audio/opus";
const OPUS_PAYLOAD_TYPE: u8 = 111;
const FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(20);

/// Comfort-noise silence, not assistant speech — same rationale as
/// `uia_openai::engine::is_silence`: unverified whether Foundry's WebRTC
/// track streams continuous silence RTP the way OpenAI's does, but filtering
/// it is harmless if it doesn't, and required if it does.
fn is_silence(pcm: &[i16]) -> bool {
    !pcm.is_empty() && pcm.iter().all(|&s| s == 0)
}

/// `POST {endpoint}/openai/v1/realtime/client_secrets` with an `api-key`
/// header — the long-lived key's only use. Reuses
/// `uia_openai::creds::parse_client_secret` for the response body since
/// both providers return `{"value": "..."}` (verified: Microsoft's own
/// sample token service reads `data.get('value', '')` from this exact
/// endpoint).
async fn mint_ephemeral_secret(
    http: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    model: &str,
) -> Result<String, EngineError> {
    let url = format!(
        "{}/openai/v1/realtime/client_secrets",
        endpoint.trim_end_matches('/')
    );
    let resp = http
        .post(url)
        .header("api-key", api_key)
        .json(&serde_json::json!({"session": {"type": "realtime", "model": model}}))
        .send()
        .await
        .map_err(|e| EngineError::Transport(e.to_string()))?;

    // Judged on the status BEFORE the body is read. An error reply has no
    // `value`, so parsing first reported every failure as "mint response
    // carried no client secret" -- a wrong endpoint (404) looked like a bad key.
    // The body is never included: an upstream error body can echo the key.
    check_mint_status(resp.status())?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| EngineError::Protocol(e.to_string()))?;
    parse_client_secret(&body)
}

/// What a mint reply's status means, in the words that point at the fix.
///
/// 4xx other than 408/429 is `Auth`, the one terminal error: a rejected key, a
/// wrong endpoint or an unknown deployment is not something backoff repairs,
/// and retrying it behind a spinner is the most misdiagnosed failure there is.
/// 408, 429 and 5xx are the provider's side and stay retryable.
fn check_mint_status(status: reqwest::StatusCode) -> Result<(), EngineError> {
    use reqwest::StatusCode;
    if status.is_success() {
        return Ok(());
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(EngineError::Auth(format!(
            "mint rejected the key ({status})"
        )));
    }
    if status == StatusCode::NOT_FOUND {
        return Err(EngineError::Auth(format!(
            "mint failed ({status}): the endpoint or deployment name is probably wrong \
             (the endpoint is the resource root, e.g. https://name.openai.azure.com, \
             with no /openai/v1)"
        )));
    }
    if status.is_client_error()
        && status != StatusCode::REQUEST_TIMEOUT
        && status != StatusCode::TOO_MANY_REQUESTS
    {
        return Err(EngineError::Auth(format!("mint failed ({status})")));
    }
    Err(EngineError::Protocol(format!("mint failed ({status})")))
}

pub struct FoundryEngine {
    endpoint: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    conn: Option<Connection>,
    /// What this deployment's handshake said about typed turns (S2) - the
    /// only authoritative source, since the deployment name carries no model
    /// information (FD4).
    capability: HandshakeCapability,
}

struct Connection {
    pc: Arc<dyn PeerConnection>,
    dc: Arc<dyn DataChannel>,
    track: Arc<TrackLocalStaticSample>,
    codec: OpusCodec,
    ssrc: u32,
    reader: tokio::task::JoinHandle<()>,
}

impl FoundryEngine {
    /// `model` is the Azure deployment name (e.g. `"gpt-realtime-2.1"`), not
    /// an OpenAI model id — Foundry's `session.model` field addresses a
    /// deployment, the resource owner's chosen name for a specific model
    /// version, not the model family itself.
    pub fn new(endpoint: String, api_key: String, model: String) -> Self {
        Self {
            endpoint,
            api_key,
            model,
            http: reqwest::Client::new(),
            conn: None,
            capability: HandshakeCapability::new(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
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

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let events = self.events.clone();
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
impl S2sEngine for FoundryEngine {
    fn id(&self) -> EngineId {
        EngineId::Foundry
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
        // A reconnect - or a redeployed model behind the same name - must not
        // inherit the previous session's claim.
        self.capability.reset();

        // 1. Ephemeral secret. The ONLY use of the long-lived key.
        let ephemeral =
            mint_ephemeral_secret(&self.http, &self.endpoint, &self.api_key, &self.model).await?;

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
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);

        // 3. Sendrecv Opus track.
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
            .create_data_channel("realtime-channel", None)
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
        let _ = tokio::time::timeout(ICE_GATHERING_TIMEOUT, gathered_rx.recv()).await;
        let offer_sdp = pc
            .local_description()
            .await
            .ok_or_else(|| EngineError::Transport("no local description".into()))?
            .unmarshal()
            .map_err(|e| EngineError::Protocol(e.to_string()))?
            .marshal();

        let calls_url = format!(
            "{}/openai/v1/realtime/calls",
            self.endpoint.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(calls_url)
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
        // Same replay as OpenAI's, from the same builder — this is a transport
        // wrapper around that protocol, not a second implementation of it.
        let history = memory_item_events(cfg);
        let tool_names = ToolNames::new(&cfg.tools);
        let dc_read: Arc<dyn DataChannel> = dc.clone();
        let dc_write: Arc<dyn DataChannel> = dc.clone();
        let capability = self.capability.clone();
        let reader = tokio::spawn(async move {
            while let Some(ev) = dc_read.poll().await {
                match ev {
                    DataChannelEvent::OnOpen => {
                        let _ = dc_write.send_text(&update.to_string()).await;
                        for item in &history {
                            let _ = dc_write.send_text(&item.to_string()).await;
                        }
                    }
                    DataChannelEvent::OnMessage(msg) => {
                        let text = String::from_utf8_lossy(&msg.data);
                        if std::env::var("UIA_DUMP_EVENTS").is_ok() {
                            eprintln!("RAW {text}");
                        }
                        // Same reading, same parser, same event as OpenAI's
                        // read task - this wrapper duplicates WebRTC, never
                        // the protocol (FD4).
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
                                        return;
                                    }
                                }
                            }
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
        self.send_json(serde_json::json!({
            "type": "conversation.item.create",
            "item": {"type": "function_call_output", "call_id": id, "output": result.content}
        }))
        .await?;
        self.send_json(serde_json::json!({"type": "response.create"}))
            .await
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
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
        // Byte-for-byte the same GA schema as OpenAI's own — see this
        // module's header note — so this reuses `uia_openai::protocol`
        // rather than re-deriving an identical event.
        self.send_json(text_turn_event(text)).await?;
        self.send_json(serde_json::json!({"type": "response.create"}))
            .await
    }

    /// The modalities this deployment echoed at handshake (FD4), which is
    /// the only thing here that can answer: a Foundry deployment name is a
    /// string the user chose in their own Azure portal and carries no model
    /// information.
    ///
    /// The fallback stays `Unknown` - unlike OpenAI, nothing has proven a
    /// typed turn against an arbitrary Azure deployment, so an uninformative
    /// handshake leaves the question genuinely open. By FD3 that keeps the
    /// control live and lets a real failure demote it.
    fn text_turn_support(&self) -> TextTurnSupport {
        self.capability.get_or(TextTurnSupport::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_mint_status_passes() {
        assert!(check_mint_status(reqwest::StatusCode::OK).is_ok());
    }

    #[test]
    fn a_wrong_endpoint_names_the_status_and_the_fix_not_a_missing_secret() {
        let e = check_mint_status(reqwest::StatusCode::NOT_FOUND).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("404"), "got {msg}");
        assert!(msg.contains("endpoint"), "got {msg}");
        assert!(!msg.contains("no client secret"), "got {msg}");
        assert!(e.is_terminal(), "a wrong endpoint must not be retried");
    }

    #[test]
    fn a_rejected_key_is_terminal() {
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            let e = check_mint_status(status).unwrap_err();
            assert!(e.is_terminal(), "{status}");
            assert!(e.to_string().contains("rejected the key"), "{status}");
        }
    }

    #[test]
    fn other_client_errors_are_terminal_but_throttling_and_server_errors_retry() {
        assert!(
            check_mint_status(reqwest::StatusCode::BAD_REQUEST)
                .unwrap_err()
                .is_terminal()
        );
        for status in [
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            reqwest::StatusCode::BAD_GATEWAY,
        ] {
            let e = check_mint_status(status).unwrap_err();
            assert!(!e.is_terminal(), "{status} should be retryable");
            assert!(e.to_string().contains(status.as_str()), "{status}");
        }
    }

    fn engine() -> FoundryEngine {
        FoundryEngine::new(
            "https://example.services.ai.azure.com".into(),
            "key".into(),
            "deployment".into(),
        )
    }

    #[test]
    fn a_deployment_that_reports_text_is_supported() {
        // The deployment name told us nothing (FD4); its handshake does.
        let e = engine();
        e.capability
            .observe(r#"{"type":"session.created","session":{"modalities":["audio","text"]}}"#);
        assert_eq!(e.text_turn_support(), TextTurnSupport::Supported);
    }

    #[test]
    fn a_deployment_that_reports_no_text_channel_is_unsupported() {
        let e = engine();
        e.capability
            .observe(r#"{"type":"session.created","session":{"modalities":["audio"]}}"#);
        let got = e.text_turn_support();
        assert!(!got.is_usable(), "got {got:?}");
        assert!(got.reason().is_some_and(|r| r.contains("speak instead")));
    }

    #[test]
    fn an_uninformative_handshake_leaves_foundry_unknown() {
        // FD5/FD3: "we could not tell" must not cost the user their input,
        // and unlike OpenAI there is no proven baseline to fall back to.
        let e = engine();
        e.capability
            .observe(r#"{"type":"session.created","session":{"type":"realtime"}}"#);
        assert_eq!(e.text_turn_support(), TextTurnSupport::Unknown);
        assert!(e.text_turn_support().is_usable());
    }

    #[test]
    fn foundry_runs_the_same_parser_as_openai_not_a_copy() {
        // The parser and the shared cell both come from
        // `uia_openai::protocol` - this wrapper duplicates WebRTC, never
        // the protocol (FD4). Asserted structurally so a future copy-paste
        // into this crate fails a test rather than drifting quietly.
        let raw = r#"{"type":"session.created","session":{"modalities":["audio"]}}"#;
        let e = engine();
        e.capability.observe(raw);
        assert_eq!(
            e.text_turn_support(),
            uia_openai::protocol::text_turn_support_from_session_created(
                &serde_json::from_str::<serde_json::Value>(raw).unwrap()
            )
        );
    }

    #[test]
    fn text_turns_are_reported_unknown_pending_handshake_detection() {
        // A deployment name is a user-chosen string in their own Azure portal
        // and carries no model information (PLAN.md FD4), so S0 has nothing
        // authoritative to report. FD3: `Unknown` is usable — the user keeps
        // their input and only a real failure takes it away.
        let e = FoundryEngine::new(
            "https://example.services.ai.azure.com".into(),
            "key".into(),
            "deployment".into(),
        );
        assert_eq!(e.text_turn_support(), TextTurnSupport::Unknown);
        assert!(
            e.text_turn_support().is_usable(),
            "Unknown must never disable the control (FD3)"
        );
    }

    #[test]
    fn reports_its_engine_id_and_declared_formats() {
        let engine = FoundryEngine::new(
            "https://example.services.ai.azure.com".into(),
            "key".into(),
            "gpt-realtime-2.1".into(),
        );
        assert_eq!(engine.id(), EngineId::Foundry);
        assert_eq!(engine.input_format().channels, 1);
        assert_eq!(engine.output_format().sample_rate_hz, 24_000);
    }

    #[tokio::test]
    async fn sending_before_connecting_is_closed_not_a_panic() {
        let mut e = FoundryEngine::new(
            "https://example.services.ai.azure.com".into(),
            "key".into(),
            "gpt-realtime-2.1".into(),
        );
        assert!(matches!(e.interrupt().await, Err(EngineError::Closed)));
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
        assert!(e.close().await.is_ok());
    }
}
