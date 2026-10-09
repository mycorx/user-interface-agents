// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Live peer-connection tests. Ignored by default: they need `OPENAI_API_KEY`
//! (or the gitignored key file) and outbound UDP.
//!
//!   cargo test -p uia-openai -- --ignored --nocapture
//!
//! The frozen test gate says `cargo test` must pass in WSL2 with no audio
//! devices and no network, which is exactly why these are `#[ignore]`d.

use std::time::Duration;

use tokio::sync::mpsc;
use uia_core::engine::{EngineEvent, S2sEngine, SessionConfig};
use uia_core::tools::ToolDescriptor;
use uia_openai::OpenAiEngine;

fn a_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "clock.now".into(),
        description: "Get the current local time for a city.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
        requires_confirmation: false,
    }
}

/// Wait for `Ready`, collecting everything seen on the way for diagnostics.
async fn await_ready(rx: &mut mpsc::Receiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let ready = matches!(ev, EngineEvent::Ready);
                seen.push(ev);
                if ready {
                    return seen;
                }
            }
            Ok(None) => return seen,
            Err(_) => return seen,
        }
    }
}

/// Collect everything that arrives within `window`.
async fn drain(rx: &mut mpsc::Receiver<EngineEvent>, window: Duration) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some(ev)) = tokio::time::timeout(
        deadline.saturating_duration_since(tokio::time::Instant::now()),
        rx.recv(),
    )
    .await
    {
        seen.push(ev);
    }
    println!("--- drained: {seen:?}");
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs OPENAI_API_KEY and network"]
async fn a_live_peer_connection_reaches_ready() {
    let mut engine = OpenAiEngine::from_env().expect("no OPENAI_API_KEY and no key file");
    let (tx, mut rx) = mpsc::channel(64);
    let cfg = SessionConfig {
        system_prompt: Some("You are Assistant. Be brief.".into()),
        tools: vec![a_tool()],
        ..SessionConfig::default()
    };

    engine.connect(&cfg, tx).await.expect("connect failed");
    let seen = await_ready(&mut rx).await;
    println!("--- events until ready: {seen:?}");
    assert!(
        seen.iter().any(|e| matches!(e, EngineEvent::Ready)),
        "never reached Ready; saw {seen:?}"
    );

    // Nothing we send may provoke a server error. A rejected event name or an
    // invalid tool declaration comes back as an `error` event, which parses to a
    // transient EngineError and would start a reconnect loop against a session
    // that is actually fine. This is how the `server.tool` name rejection and
    // the transcription option get proven, not assumed.
    engine.interrupt().await.expect("interrupt send failed");
    for e in drain(&mut rx, Duration::from_secs(5)).await {
        if let EngineEvent::Error(e) = e {
            panic!("the session reported an error: {e}");
        }
    }

    engine.close().await.expect("close failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs OPENAI_API_KEY and network"]
async fn a_live_session_accepts_the_transcription_option() {
    // The transcription model constant is otherwise UNVERIFIED: S1 sent no
    // microphone audio, so no input-transcription event was ever observed.
    let mut engine = OpenAiEngine::from_env().expect("no OPENAI_API_KEY and no key file");
    let (tx, mut rx) = mpsc::channel(64);
    let cfg = SessionConfig {
        transcription: true,
        ..SessionConfig::default()
    };

    engine.connect(&cfg, tx).await.expect("connect failed");
    let seen = await_ready(&mut rx).await;
    assert!(seen.iter().any(|e| matches!(e, EngineEvent::Ready)));

    for e in drain(&mut rx, Duration::from_secs(5)).await {
        if let EngineEvent::Error(e) = e {
            panic!("the transcription option was rejected: {e}");
        }
    }
    // NOTE: acceptance of the option is all this proves. The event a *completed*
    // transcription produces is settled by the audio test below, which is the
    // first thing in this project to send speech.

    engine.close().await.expect("close failed");
}

/// 20 ms of 24 kHz mono — one frame at the engine's boundary rate.
const FRAME: usize = 480;

/// Pinned like every other model id in this project: a floating alias would
/// change the fixture under a rerun.
const TTS_MODEL: &str = "gpt-4o-mini-tts-2025-12-15";

/// Synthesise real speech to send.
///
/// A synthetic tone will not do: OpenAI's `server_vad` classifies speech, and a
/// sine wave is never classified as any. `response_format: "pcm"` returns raw
/// 24 kHz mono signed-16-bit little-endian samples — exactly the engine's
/// boundary format, so nothing has to be resampled to build the fixture.
async fn synthesize_speech(api_key: &str, text: &str) -> Vec<i16> {
    let resp = reqwest::Client::new()
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "model": TTS_MODEL,
            "input": text,
            "voice": "alloy",
            "response_format": "pcm",
        }))
        .send()
        .await
        .expect("tts request failed");
    assert!(resp.status().is_success(), "tts returned {}", resp.status());
    let bytes = resp.bytes().await.expect("tts body");
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs OPENAI_API_KEY and network"]
async fn a_live_session_hears_real_audio_and_speaks_back() {
    // THE test for S16. Everything else in this file proves the session is
    // healthy; only this one proves audio actually moves, in either direction.
    // A round trip through our own codec proves portability, not
    // interoperability — this is what proves OpenAI's decoder accepts what
    // `uia-audio` produces, and that our decoder accepts what theirs sends.
    let api_key = uia_openai::creds::resolve_api_key_from_env().expect("no key");
    let speech = synthesize_speech(&api_key, "Hello. What is two plus two? Answer briefly.").await;
    println!(
        "--- fixture: {} samples of speech ({} ms)",
        speech.len(),
        speech.len() / 24
    );
    assert!(
        speech.len() > FRAME * 10,
        "tts fixture is implausibly short"
    );

    let mut engine = OpenAiEngine::from_env().expect("no key");
    let (tx, mut rx) = mpsc::channel(1024);
    let cfg = SessionConfig {
        system_prompt: Some("You are Assistant. Answer in one short sentence.".into()),
        transcription: true,
        ..SessionConfig::default()
    };
    engine.connect(&cfg, tx).await.expect("connect failed");
    let seen = await_ready(&mut rx).await;
    assert!(
        seen.iter().any(|e| matches!(e, EngineEvent::Ready)),
        "never reached Ready; saw {seen:?}"
    );

    // Pace at real time. Blasting the whole clip at once gives the server VAD
    // no time base to detect a turn in, and `write_sample` packetises
    // immediately rather than pacing for us.
    let mut sent = 0usize;
    for frame in speech.chunks(FRAME) {
        engine.send_audio(frame).await.expect("send_audio failed");
        sent += frame.len();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Trailing silence so VAD sees the turn end (silence_duration_ms is 500).
    for _ in 0..50 {
        engine.send_audio(&[0i16; FRAME]).await.expect("silence");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!("--- sent {sent} samples of speech + 1000 ms silence");

    let events = drain(&mut rx, Duration::from_secs(20)).await;
    for e in &events {
        if let EngineEvent::Error(err) = e {
            panic!("the session reported an error: {err}");
        }
    }

    let decoded: Vec<i16> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::AudioChunk(pcm) => Some(pcm.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    let audio = decoded.len();
    let heard: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::UserTranscript(t) => Some(t),
            _ => None,
        })
        .collect();
    let spoke: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ModelTranscript(t) => Some(t),
            _ => None,
        })
        .collect();
    println!("--- UserTranscript: {heard:?}");
    println!("--- ModelTranscript: {spoke:?}");
    println!(
        "--- decoded {audio} samples of assistant audio ({} ms)",
        audio / 24
    );

    assert!(
        !heard.is_empty(),
        "no UserTranscript: the session never transcribed our speech, so it did \
         not hear the audio we encoded. Events: {events:?}"
    );
    assert!(
        audio > 0,
        "no AudioChunk decoded: on_track never delivered the assistant's voice. \
         Events: {events:?}"
    );

    // Both directions must be proven against a third party, not against
    // ourselves. The send direction is proven above: OpenAI transcribed the
    // speech we encoded. For the receive direction, `decode` returning `Ok`
    // proves nothing -- opus-rs's broken paths return confident noise, not
    // errors -- so read our decoded PCM back with Whisper and check the words.
    let read_back = transcribe(&api_key, &decoded).await;
    println!("--- our decoded audio, transcribed back: {read_back:?}");
    // Accept either spelling: Whisper writes the answer as digits about as often
    // as words ("2 plus 2 equals 4."), and which one it picks says nothing about
    // whether our decode worked.
    let said_four = {
        let t = read_back.to_lowercase();
        t.contains("four") || t.contains('4')
    };
    assert!(
        said_four,
        "the audio we decoded from the remote track is not intelligible: Whisper \
         read it as {read_back:?}, but the assistant said {spoke:?}. The Opus \
         decode path is producing noise, not speech."
    );

    engine.close().await.expect("close failed");
}

/// Wrap 24 kHz mono PCM in a minimal WAV container so it can be posted to the
/// transcription endpoint, which sniffs the format rather than taking a hint.
fn wav_24k_mono(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    w.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // channels
    w.extend_from_slice(&24_000u32.to_le_bytes()); // sample rate
    w.extend_from_slice(&48_000u32.to_le_bytes()); // byte rate = rate * 2
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// Transcribe audio we decoded ourselves.
///
/// This is the only thing that proves the RECEIVE direction interoperates. That
/// `decode` returned `Ok` proves nothing: `opus-rs`'s CELT path returns
/// confident noise rather than an error, so unintelligible audio would look
/// exactly like working audio from inside the process. Sending it back to a
/// third party to read aloud is the check that cannot be fooled.
async fn transcribe(api_key: &str, pcm: &[i16]) -> String {
    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part(
            "file",
            reqwest::multipart::Part::bytes(wav_24k_mono(pcm))
                .file_name("assistant.wav")
                .mime_str("audio/wav")
                .expect("mime"),
        );
    let resp = reqwest::Client::new()
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .expect("transcription request failed");
    assert!(
        resp.status().is_success(),
        "transcription returned {}",
        resp.status()
    );
    resp.json::<serde_json::Value>()
        .await
        .expect("transcription body")
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string()
}
