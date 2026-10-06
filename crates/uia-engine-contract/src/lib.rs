// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use std::time::Duration;
use uia_core::audio::Resampler;
use uia_core::engine::{EngineEvent, S2sEngine, SessionConfig};
use uia_core::tools::{ToolDescriptor, ToolResult};

/// The rate OpenAI TTS returns and what `provider_ab.rs` already proved works
/// as a fixture rate for both engines (Nova's 16 kHz input is reached by
/// resampling down, same as the real app does per session).
pub const SPEECH_SAMPLE_RATE_HZ: u32 = 24_000;

/// The tool this contract declares. Named with a dot deliberately: it
/// exercises `uia-mcp`'s `.` -> `__` wire-name sanitisation and its
/// restore path, the same as any real `server.tool` name would.
pub fn clock_tool() -> ToolDescriptor {
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

/// Instructs the assistant to call `clock_tool()` the moment it hears a
/// city-time question — the same instruction `provider_ab.rs` already proved
/// reliably triggers a tool call on both providers (10/10 across S14's A/B).
pub const CLOCK_TOOL_SYSTEM_PROMPT: &str = "You are a test harness. When the \
    user asks what time it is somewhere, you MUST call the clock.now tool \
    with that city as the argument, then answer in one short sentence.";

/// Invariants every engine must satisfy, regardless of provider. Run live
/// against real credentials; ignored by default so `cargo test` stays offline.
///
/// `speech_24k` is audio at `SPEECH_SAMPLE_RATE_HZ` that should make the
/// assistant call `clock_tool()` — e.g. TTS of "What time is it in Sydney?".
/// Pass an empty slice against a scripted fake engine whose `ToolCall` is
/// already queued on connect: the wait loop below drains whatever is on the
/// channel regardless of whether any speech was actually sent.
pub async fn assert_engine_contract(mut engine: Box<dyn S2sEngine>, speech_24k: &[i16]) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);

    let cfg = SessionConfig {
        system_prompt: Some(CLOCK_TOOL_SYSTEM_PROMPT.into()),
        tools: vec![clock_tool()],
        ..SessionConfig::default()
    };

    engine.connect(&cfg, tx).await.expect(
        "connect must succeed with a declared tool — a tool-name character-set \
         rejection (the S14 Nova defect) would fail here",
    );

    let ready = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("engine must emit an event within 10s")
        .expect("channel must not close before Ready");
    assert!(
        matches!(ready, EngineEvent::Ready),
        "first event must be Ready, got {ready:?}"
    );

    // Declared formats must be sane, since the pipeline resamples to them.
    let input_rate = engine.input_format().sample_rate_hz;
    assert!(input_rate >= 8_000);
    assert!(engine.output_format().sample_rate_hz >= 8_000);
    assert_eq!(
        engine.input_format().channels,
        1,
        "pipeline assumes mono input"
    );

    // Speak the driving audio, resampled to this engine's own input rate —
    // never assume 24 kHz, the same rule the real app's per-session
    // `Resampler` follows.
    let speech = if speech_24k.is_empty() {
        Vec::new()
    } else if input_rate == SPEECH_SAMPLE_RATE_HZ {
        speech_24k.to_vec()
    } else {
        Resampler::new(SPEECH_SAMPLE_RATE_HZ, input_rate, 1024)
            .expect("resampler")
            .process(speech_24k)
            .expect("resample")
    };
    let frame_len = (input_rate as u64 * 20 / 1000).max(1) as usize;
    for f in speech.chunks(frame_len) {
        let _ = engine.send_audio(f).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Wait for the tool call, feeding silence meanwhile so a live provider's
    // VAD sees a continuous stream. A scripted fake engine has already queued
    // its ToolCall by now, so this returns on the very first poll.
    let silence = vec![0i16; frame_len];
    let (call_id, call_name, _args) = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let _ = engine.send_audio(&silence).await;
            match tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
                Ok(Some(EngineEvent::ToolCall { id, name, args })) => return (id, name, args),
                Ok(Some(_)) | Err(_) => continue,
                Ok(None) => panic!("channel closed before a ToolCall"),
            }
        }
    })
    .await
    .expect("engine must emit a ToolCall within 20s of hearing the tool prompt");

    // Assert on the `ToolDescriptor` name the executor receives, never the
    // wire name: PLAN.md's frozen decision is that both engines now produce
    // the *same* sanitised wire name and restore it before this event fires.
    assert_eq!(
        call_name, "clock.now",
        "ToolCall must carry the ToolDescriptor name, not the sanitised wire name"
    );

    // Exercises Nova's stringified-JSON + `text/plain` requirement
    // structurally for the first time: a bare sentence here previously killed
    // the whole Nova session (`ValidationException: Tool Response parsing
    // error`).
    engine
        .send_tool_result(call_id, ToolResult::ok(r#"{"time":"3:00 PM"}"#))
        .await
        .expect("engine must accept the tool result");

    engine
        .send_audio(&vec![0i16; frame_len])
        .await
        .expect("silence must be accepted");
    engine
        .interrupt()
        .await
        .expect("interrupt must be accepted even when not speaking");
    engine.close().await.expect("close must succeed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use uia_core::engine::FakeEngine;

    /// The fake path the acceptance criteria require: no real service, no
    /// network, and no driving speech — `FakeEngine` queues its whole script
    /// (including the `ToolCall`) the moment `connect()` runs.
    #[tokio::test]
    async fn the_contract_passes_against_a_scripted_fake_engine() {
        let engine = FakeEngine::new(vec![
            EngineEvent::Ready,
            EngineEvent::ToolCall {
                id: "call-1".into(),
                name: "clock.now".into(),
                args: serde_json::json!({"city": "Sydney"}),
            },
        ]);
        assert_engine_contract(Box::new(engine), &[]).await;
    }
}
