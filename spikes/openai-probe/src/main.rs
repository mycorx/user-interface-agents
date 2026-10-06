//! Throwaway spike: prove the OpenAI Realtime WebRTC handshake from Rust and
//! learn the real server-event shapes.
//!
//! Not shipped code. It mints an ephemeral client secret with the long-lived key,
//! builds a peer connection with a sendrecv Opus track and an `oai-events` data
//! channel, completes the SDP offer/answer against the Realtime API, then dumps
//! every server event verbatim.
//!
//! The key is read from OPENAI_API_KEY, falling back to the gitignored key file.
//! It is never printed.
//!
//! API NOTE: `webrtc` 0.20 is the sans-IO rewrite. There is no `APIBuilder`, no
//! `on_message` callback and no `gathering_complete_promise`. You build with
//! `PeerConnectionBuilder`, observe ICE gathering through a
//! `PeerConnectionEventHandler`, and *poll* the data channel for events. Codec
//! parameter types are not re-exported by the facade, so `rtc` is a direct
//! dependency too.

use std::sync::Arc;

use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState,
    RTCSessionDescription, Registry, register_default_interceptors,
};

const MODEL: &str = "gpt-realtime-2.1";
const MIME_TYPE_OPUS: &str = "audio/opus";

fn load_key() -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(k) = std::env::var("OPENAI_API_KEY")
        && !k.trim().is_empty()
    {
        return Ok(k.trim().to_string());
    }
    // Fallback: the gitignored local key file, two levels up from the spike.
    for path in ["../../gpt-api.key", "gpt-api.key"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            return Ok(s.trim().to_string());
        }
    }
    Err("no OPENAI_API_KEY and no gpt-api.key found".into())
}

/// Keep the huge base64 audio deltas out of the log.
fn summarize(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(mut v) => {
            if let Some(delta) = v.get_mut("delta")
                && let Some(s) = delta.as_str()
                && s.len() > 120
            {
                let n = s.len();
                *delta = serde_json::Value::String(format!("<{n} base64 chars elided>"));
            }
            v.to_string()
        }
        Err(_) => raw.to_string(),
    }
}

#[derive(Clone)]
struct Handler {
    gather_tx: mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        println!("--- ICE gathering: {state:?}");
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_tx.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        println!("--- pc state: {state}");
    }

    async fn on_track(&self, track: Arc<dyn webrtc::media_stream::track_remote::TrackRemote>) {
        let ssrcs = track.ssrcs().await;
        let codec = match ssrcs.first() {
            Some(ssrc) => track.codec(*ssrc).await,
            None => None,
        };
        println!(
            "REMOTE TRACK: track_id={} kind={:?} ssrcs={ssrcs:?} codec={:?}",
            track.track_id().await,
            track.kind().await,
            codec.map(|c| (c.mime_type, c.clock_rate, c.channels))
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = load_key()?;
    let http = reqwest::Client::new();

    // --- 1. ephemeral client secret ---------------------------------------
    let secret: serde_json::Value = http
        .post("https://api.openai.com/v1/realtime/client_secrets")
        .bearer_auth(&key)
        .json(&serde_json::json!({
            "session": { "type": "realtime", "model": MODEL }
        }))
        .send()
        .await?
        .json()
        .await?;

    let ephemeral = secret["value"]
        .as_str()
        .or_else(|| secret["client_secret"]["value"].as_str())
        .ok_or_else(|| format!("no ephemeral secret in response: {secret}"))?
        .to_string();
    println!(
        "minted ephemeral secret ({} chars), expires_at={}",
        ephemeral.len(),
        secret["expires_at"]
    );
    // The session object the server echoes back IS the negotiated config.
    println!(
        "SESSION CONFIG: {}",
        serde_json::to_string(&secret["session"])?
    );

    // --- 2. peer connection ------------------------------------------------
    let mut media_engine = MediaEngine::default();
    let audio_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    };
    media_engine.register_codec(audio_codec.clone(), RtpCodecKind::Audio)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }])
        .build();

    let (gather_tx, mut gather_rx) = mpsc::channel::<()>(1);
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler { gather_tx }))
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await?;

    // Sendrecv Opus audio: WebRTC carries Opus on the wire, never raw PCM.
    let ssrc = rand::random::<u32>();
    let track: Arc<TrackLocalStaticSample> = Arc::new(TrackLocalStaticSample::new(
        MediaStreamTrack::new(
            "uia-probe-stream".to_owned(),
            "uia-probe-audio".to_owned(),
            "uia-probe-audio".to_owned(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: audio_codec.rtp_codec.clone(),
                ..Default::default()
            }],
        ),
    )?);
    pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

    // --- 3. the events data channel ---------------------------------------
    let dc = pc.create_data_channel("oai-events", None).await?;

    // --- 4. offer, full ICE gather, then SDP exchange ----------------------
    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;
    // Sending a half-gathered offer is the classic hang; wait for Complete.
    let _ = gather_rx.recv().await;
    let offer_sdp = pc
        .local_description()
        .await
        .ok_or("no local description")?
        .unmarshal()?
        .marshal();
    println!("--- offer gathered ({} bytes)", offer_sdp.len());

    let resp = http
        .post(format!(
            "https://api.openai.com/v1/realtime/calls?model={MODEL}"
        ))
        .bearer_auth(&ephemeral)
        .header("Content-Type", "application/sdp")
        .body(offer_sdp)
        .send()
        .await?;
    let status = resp.status();
    let answer_sdp = resp.text().await?;
    println!("--- SDP exchange: HTTP {status}, answer {} bytes", answer_sdp.len());
    if !status.is_success() {
        println!("ANSWER BODY: {answer_sdp}");
        return Ok(());
    }

    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;

    // --- 5. drive the data channel and dump every server event -------------
    let dc_poll: Arc<dyn DataChannel> = dc.clone();
    let mut answered_tool = false;
    let mut responses_done = 0;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(60);
    loop {
        let ev = tokio::select! {
            e = dc_poll.poll() => e,
            _ = tokio::time::sleep_until(deadline) => { println!("--- deadline reached"); break; }
        };
        let Some(ev) = ev else {
            println!("--- data channel closed");
            break;
        };
        match ev {
            DataChannelEvent::OnOpen => {
                println!("--- data channel open");
                // Declare a tool so the function-call event shape shows up too.
                let update = serde_json::json!({
                    "type": "session.update",
                    "session": {
                        "type": "realtime",
                        "tools": [{
                            "type": "function",
                            "name": "get_time",
                            "description": "Get the current local time for a city.",
                            "parameters": {
                                "type": "object",
                                "properties": {"city": {"type": "string"}},
                                "required": ["city"]
                            }
                        }],
                        "tool_choice": "auto"
                    }
                });
                dc.send_text(&update.to_string()).await?;
                // Force a response without a microphone: a text item, then response.create.
                let item = serde_json::json!({
                    "type": "conversation.item.create",
                    "item": {
                        "type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "What time is it in Sydney?"}]
                    }
                });
                dc.send_text(&item.to_string()).await?;
                dc.send_text(&serde_json::json!({"type": "response.create"}).to_string())
                    .await?;
            }
            DataChannelEvent::OnMessage(msg) => {
                let text = String::from_utf8_lossy(&msg.data).to_string();
                println!("EVENT: {}", summarize(&text));

                // Complete the tool round trip so the full cycle is on record.
                if !answered_tool
                    && text.contains("\"response.function_call_arguments.done\"")
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                {
                    answered_tool = true;
                    let call_id = v["call_id"].as_str().unwrap_or_default().to_string();
                    let out = serde_json::json!({
                        "type": "conversation.item.create",
                        "item": {
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": "{\"time\":\"09:20\",\"city\":\"Sydney\"}"
                        }
                    });
                    dc.send_text(&out.to_string()).await?;
                    dc.send_text(&serde_json::json!({"type": "response.create"}).to_string())
                        .await?;
                }

                if text.contains("\"response.done\"") {
                    responses_done += 1;
                    // One response if no tool was called; two if the tool round trip ran.
                    if responses_done >= 2 || !answered_tool {
                        println!("--- final response.done seen; closing");
                        break;
                    }
                }
            }
            DataChannelEvent::OnClose => {
                println!("--- data channel closed");
                break;
            }
            other => println!("--- dc event: {other:?}"),
        }
    }

    pc.close().await?;
    Ok(())
}
