// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! S14's provider A/B, run headlessly.
//!
//! # Why this is not a microphone
//!
//! The stage asks for time-to-first-audio and tool-calling reliability over at
//! least ten exchanges per engine. Measured through a human at a microphone,
//! neither number is reproducible: the prompt wording, the pace of speech, the
//! room, and the moment the hotkey is released all move the result more than
//! the engines differ from each other. So this harness does what S16 proved
//! works — synthesises the prompt with OpenAI TTS and streams it at real-time
//! pacing — which makes every exchange byte-identical across engines and across
//! reruns. A sine wave would not do: `server_vad` classifies *speech*, and a
//! tone is never classified as any.
//!
//! What this therefore does NOT measure, and `docs/MANUAL-TEST.md` covers
//! instead: the capture device, the hotkey, the overlay, echo cancellation, and
//! everything else between a human and `send_audio`.
//!
//!   OPENAI_API_KEY=... cargo test -p uia-app --test provider_ab \
//!       -- --ignored --nocapture --test-threads=1
//!
//! `AWS_PROFILE=sso-nonprod` is additionally required for the Nova runs.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use uia_core::audio::{Resampler, rms};
use uia_core::engine::{EngineEvent, S2sEngine, SessionConfig};
use uia_core::tools::{ToolDescriptor, ToolResult};

/// 20 ms at 24 kHz — the rate OpenAI TTS returns and OpenAI Realtime consumes.
const TTS_RATE_HZ: u32 = 24_000;
const FRAME_MS: u64 = 20;

/// Pinned for the same reason every other model id here is: a floating alias
/// silently changes the fixture under a rerun and makes the A/B unrepeatable.
const TTS_MODEL: &str = "gpt-4o-mini-tts-2025-12-15";

/// Ten prompts that should each call the tool exactly once, with the city as
/// the argument. Deliberately varied in phrasing — asking the same sentence ten
/// times measures determinism, not reliability.
const TOOL_PROMPTS: [(&str, &str); 10] = [
    ("What time is it in Tokyo right now?", "Tokyo"),
    ("Tell me the current time in Sydney.", "Sydney"),
    ("I need the local time for London.", "London"),
    ("What's the time over in Paris at the moment?", "Paris"),
    ("Could you check the clock for New York?", "New York"),
    ("How late is it in Berlin?", "Berlin"),
    ("Give me the current time in Singapore.", "Singapore"),
    ("Do you know what time it is in Toronto?", "Toronto"),
    ("Check the time in Dublin for me.", "Dublin"),
    ("What is the local time in Madrid?", "Madrid"),
];

const SYSTEM_PROMPT: &str = "You are Assistant, a voice assistant. When the user \
     asks what time it is somewhere, you MUST call the clock.now tool with that \
     city. Then answer in one short sentence.";

fn clock_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "clock.now".into(),
        description: "Get the current local time for a city.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string", "description": "City name"}},
            "required": ["city"]
        }),
        requires_confirmation: false,
    }
}

async fn synthesize(api_key: &str, text: &str) -> Vec<i16> {
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
    resp.bytes()
        .await
        .expect("tts body")
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// Strip trailing near-silence from a TTS clip.
///
/// Without this the measurement is wrong in a way that looks like a great
/// result: `gpt-4o-mini-tts` pads the end of every clip, both providers' VADs
/// start their end-of-turn timer during that padding, and the assistant can
/// begin speaking BEFORE the send loop has finished streaming the fixture. The
/// first run of this harness produced a 5 ms time-to-first-audio that way —
/// not a fast model, a clock started after the answer had already begun.
fn trim_trailing_silence(pcm: &[i16]) -> &[i16] {
    const WINDOW: usize = 240; // 10 ms at 24 kHz
    let mut end = pcm.len();
    while end >= WINDOW && rms(&pcm[end - WINDOW..end]) < 0.005 {
        end -= WINDOW;
    }
    &pcm[..end]
}

/// TTS returns 24 kHz; Nova takes 16 kHz. Resampled once per fixture, not per
/// frame — S16 found the resampler carries filter state across calls and a
/// fresh one per frame both destroys the signal and swallows short frames.
fn to_rate(pcm: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to {
        return pcm.to_vec();
    }
    let mut r = Resampler::new(from, to, 1024).expect("resampler");
    r.process(pcm).expect("resample")
}

/// What one exchange produced.
#[derive(Debug)]
struct Exchange {
    /// End of the speech we sent -> first assistant audio with real signal in
    /// it. Includes the provider's own end-of-turn silence detection, which is
    /// part of what a user waits through and is called out in the writeup.
    time_to_first_audio: Option<Duration>,
    time_to_first_transcript: Option<Duration>,
    /// The tool was called, and the `city` argument matched the prompt.
    tool_called_correctly: bool,
    tool_called_at_all: bool,
    errored: Option<String>,
}

impl Exchange {
    fn failed(why: impl Into<String>) -> Self {
        Self {
            time_to_first_audio: None,
            time_to_first_transcript: None,
            tool_called_correctly: false,
            tool_called_at_all: false,
            errored: Some(why.into()),
        }
    }
}

/// Run one prompt against one engine, from a fresh connection.
///
/// A fresh session per exchange is deliberate: a shared session would let turn
/// N's context change turn N+1's latency, and the engines cache differently.
async fn one_exchange(
    engine: &mut dyn S2sEngine,
    speech_24k: &[i16],
    expected_city: &str,
) -> Exchange {
    let rate = engine.input_format().sample_rate_hz;
    let speech = to_rate(speech_24k, TTS_RATE_HZ, rate);
    let frame = (rate as u64 * FRAME_MS / 1000) as usize;

    let (tx, mut rx) = mpsc::channel(4096);
    let cfg = SessionConfig {
        system_prompt: Some(SYSTEM_PROMPT.into()),
        tools: vec![clock_tool()],
        ..SessionConfig::default()
    };
    if let Err(e) = engine.connect(&cfg, tx).await {
        return Exchange::failed(format!("connect: {e}"));
    }

    // Wait for Ready before speaking; a turn sent into a half-open session is
    // measuring setup, not response latency.
    let ready_by = Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout(
            ready_by.saturating_duration_since(Instant::now()),
            rx.recv(),
        )
        .await
        {
            Ok(Some(EngineEvent::Ready)) => break,
            Ok(Some(_)) => continue,
            _ => return Exchange::failed("never reached Ready"),
        }
    }

    for f in speech.chunks(frame) {
        let _ = engine.send_audio(f).await;
        tokio::time::sleep(Duration::from_millis(FRAME_MS)).await;
    }
    // The clock starts at the end of the SPEECH, not the end of the silence:
    // the silence is what the provider's VAD needs to decide the turn ended,
    // and a user waits through it too.
    let spoke_at = Instant::now();
    let silence = vec![0i16; frame];
    let mut ex = Exchange {
        time_to_first_audio: None,
        time_to_first_transcript: None,
        tool_called_correctly: false,
        tool_called_at_all: false,
        errored: None,
    };

    // Keep feeding silence while waiting — a real microphone never stops, and
    // both providers' VADs expect a continuous stream.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut pending_tool: Option<(String, ToolResult)> = None;
    while Instant::now() < deadline {
        if let Some((id, result)) = pending_tool.take() {
            let _ = engine.send_tool_result(id, result).await;
        }
        let _ = engine.send_audio(&silence).await;
        match tokio::time::timeout(Duration::from_millis(FRAME_MS), rx.recv()).await {
            Err(_) => continue, // nothing this frame; keep the stream alive
            Ok(None) => break,
            Ok(Some(ev)) => match ev {
                EngineEvent::AudioChunk(pcm) => {
                    // Both engines emit some near-silent audio; the first chunk
                    // with real signal is what a user actually hears.
                    if ex.time_to_first_audio.is_none() && rms(&pcm) > 0.002 {
                        ex.time_to_first_audio = Some(spoke_at.elapsed());
                    }
                }
                EngineEvent::ModelTranscript(_) => {
                    if ex.time_to_first_transcript.is_none() {
                        ex.time_to_first_transcript = Some(spoke_at.elapsed());
                    }
                }
                EngineEvent::ToolCall { id, name, args } => {
                    ex.tool_called_at_all = true;
                    let city = args.get("city").and_then(|c| c.as_str()).unwrap_or("");
                    // Case-tolerant: "new york" is a correct answer for
                    // "New York", and scoring it wrong would measure the
                    // model's capitalisation, not its tool use.
                    ex.tool_called_correctly = name.contains("clock")
                        && city.to_lowercase() == expected_city.to_lowercase();
                    pending_tool = Some((
                        id,
                        ToolResult::ok(format!("The current time in {city} is 3:00 PM.")),
                    ));
                }
                EngineEvent::Error(e) => {
                    ex.errored.get_or_insert(e.to_string());
                }
                _ => {}
            },
        }
        // Everything asked for has arrived; stop paying for silence.
        if ex.time_to_first_audio.is_some() && ex.tool_called_at_all {
            break;
        }
    }

    let _ = engine.close().await;
    ex
}

fn summarise(label: &str, runs: &[Exchange]) {
    let mut ttfa: Vec<u128> = runs
        .iter()
        .filter_map(|r| r.time_to_first_audio.map(|d| d.as_millis()))
        .collect();
    ttfa.sort_unstable();
    let n = ttfa.len();
    let median = if n == 0 { 0 } else { ttfa[n / 2] };
    let mean = if n == 0 {
        0
    } else {
        ttfa.iter().sum::<u128>() / n as u128
    };
    let called = runs.iter().filter(|r| r.tool_called_at_all).count();
    let correct = runs.iter().filter(|r| r.tool_called_correctly).count();
    let errors: Vec<&str> = runs.iter().filter_map(|r| r.errored.as_deref()).collect();

    println!("\n=== {label} ===");
    println!("exchanges              : {}", runs.len());
    println!("time-to-first-audio    : n={n} median={median} ms mean={mean} ms all={ttfa:?}");
    println!(
        "tool called            : {called}/{} ({correct}/{} with the right city)",
        runs.len(),
        runs.len()
    );
    if !errors.is_empty() {
        println!("errors                 : {errors:?}");
    }
}

async fn run_openai(model: &str, label: &str) {
    let key = uia_openai::creds::resolve_api_key_from_env().expect("no OPENAI_API_KEY");
    let mut runs = Vec::new();
    for (prompt, city) in TOOL_PROMPTS {
        let speech = synthesize(&key, prompt).await;
        let speech = trim_trailing_silence(&speech);
        let mut engine = uia_openai::OpenAiEngine::new(key.clone(), model.to_string());
        let ex = one_exchange(&mut engine, speech, city).await;
        println!("  [{label}] {prompt:?} -> {ex:?}");
        runs.push(ex);
    }
    summarise(label, &runs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live A/B: needs OPENAI_API_KEY and network"]
async fn openai_mini_provider_ab() {
    run_openai(
        uia_openai::engine::DEFAULT_MODEL,
        "openai gpt-realtime-2.1-mini",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live A/B: needs OPENAI_API_KEY and network"]
async fn openai_full_provider_ab() {
    // The other half of the stage's comparison: mini is the default at ~3.2x
    // cheaper on audio, and this is where a quality or latency gap justifying
    // the full model would show up.
    run_openai("gpt-realtime-2.1", "openai gpt-realtime-2.1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live A/B: needs OPENAI_API_KEY, AWS credentials and network"]
async fn nova_provider_ab() {
    let key = uia_openai::creds::resolve_api_key_from_env().expect("no OPENAI_API_KEY");
    let region = uia_nova::NovaEngine::DEFAULT_REGION;
    let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").expect("no AWS_ACCESS_KEY_ID");
    let secret_access_key =
        std::env::var("AWS_SECRET_ACCESS_KEY").expect("no AWS_SECRET_ACCESS_KEY");
    let mut runs = Vec::new();
    for (prompt, city) in TOOL_PROMPTS {
        let speech = synthesize(&key, prompt).await;
        let speech = trim_trailing_silence(&speech);
        let mut engine = uia_nova::NovaEngine::new(
            region.to_string(),
            access_key_id.clone(),
            secret_access_key.clone(),
            uia_nova::DEFAULT_MODEL_ID.into(),
        );
        let ex = one_exchange(&mut engine, speech, city).await;
        println!("  [nova {region}] {prompt:?} -> {ex:?}");
        runs.push(ex);
    }
    summarise(&format!("nova-2-sonic ({region})"), &runs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live A/B: needs AZURE_AI_FOUNDRY_ENDPOINT / AZURE_AI_FOUNDRY_API_KEY / AZURE_AI_FOUNDRY_DEPLOYMENT, and OPENAI_API_KEY to synthesise driving speech"]
async fn foundry_provider_ab() {
    let openai_key = uia_openai::creds::resolve_api_key_from_env().expect("no OPENAI_API_KEY");
    let endpoint =
        std::env::var("AZURE_AI_FOUNDRY_ENDPOINT").expect("no AZURE_AI_FOUNDRY_ENDPOINT");
    let api_key = std::env::var("AZURE_AI_FOUNDRY_API_KEY").expect("no AZURE_AI_FOUNDRY_API_KEY");
    let deployment =
        std::env::var("AZURE_AI_FOUNDRY_DEPLOYMENT").expect("no AZURE_AI_FOUNDRY_DEPLOYMENT");
    let mut runs = Vec::new();
    for (prompt, city) in TOOL_PROMPTS {
        let speech = synthesize(&openai_key, prompt).await;
        let speech = trim_trailing_silence(&speech);
        let mut engine =
            uia_foundry::FoundryEngine::new(endpoint.clone(), api_key.clone(), deployment.clone());
        let ex = one_exchange(&mut engine, speech, city).await;
        println!("  [foundry {deployment}] {prompt:?} -> {ex:?}");
        runs.push(ex);
    }
    summarise(&format!("azure-ai-foundry ({deployment})"), &runs);
}
