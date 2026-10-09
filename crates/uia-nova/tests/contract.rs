// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

/// Synthesises "What time is it in Sydney?" via OpenAI TTS — the same
/// technique `uia-app`'s `provider_ab.rs` and `uia-openai`'s live
/// contract test use, reused here rather than checking in a static fixture so
/// the clip stays byte-reproducible from the prompt text alone. Nova's own
/// credentials cannot synthesise speech, so this test needs both providers'
/// keys — matching `provider_ab.rs`'s `nova_provider_ab`, which already does.
async fn synthesize_sydney_time_question(api_key: &str) -> Vec<i16> {
    let resp = reqwest::Client::new()
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "model": "gpt-4o-mini-tts-2025-12-15",
            "input": "What time is it in Sydney?",
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

#[tokio::test]
#[ignore = "live: requires AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY with Bedrock access, and OPENAI_API_KEY to synthesise the driving speech"]
async fn nova_satisfies_the_engine_contract() {
    let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID must be set");
    let secret_access_key =
        std::env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY must be set");
    let openai_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");
    let speech = synthesize_sydney_time_question(&openai_key).await;
    let engine = uia_nova::NovaEngine::new(
        uia_nova::NovaEngine::DEFAULT_REGION.into(),
        access_key_id,
        secret_access_key,
        uia_nova::DEFAULT_MODEL_ID.into(),
    );
    uia_engine_contract::assert_engine_contract(Box::new(engine), &speech).await;
}

/// SP2 Task 3: `classify_sdk_error`'s auth-keyword list was inferred from the
/// SDK's exception *type* names, never checked against what Bedrock actually
/// puts on the wire. Misclassifying an auth rejection as `Transport` is not
/// cosmetic — `EngineError::is_terminal` drives the reconnect loop, so a
/// wrong answer means retrying forever against a credential that will never
/// work. Deliberately-invalid credentials, so this needs network but no real
/// AWS account.
#[tokio::test]
#[ignore = "live: reaches Bedrock with deliberately-invalid AWS credentials to provoke a real auth rejection"]
async fn an_invalid_credential_pair_is_classified_as_auth_not_transport() {
    use uia_core::engine::{EngineEvent, S2sEngine, SessionConfig};

    let mut engine = uia_nova::NovaEngine::new(
        uia_nova::NovaEngine::DEFAULT_REGION.into(),
        "AKIAINVALIDINVALIDX".into(),
        "invalid-secret-access-key-value-000000000000000000".into(),
        uia_nova::DEFAULT_MODEL_ID.into(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let cfg = SessionConfig::default();

    // `connect` itself may return the error, or it may surface asynchronously
    // as an `EngineEvent::Error` on the channel — the contract doesn't
    // guarantee which, so check both paths.
    let connect_err = engine.connect(&cfg, tx).await.err();
    let channel_err = if connect_err.is_none() {
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .ok()
            .flatten()
            .and_then(|ev| match ev {
                EngineEvent::Error(e) => Some(e),
                _ => None,
            })
    } else {
        None
    };

    let err = connect_err
        .or(channel_err)
        .expect("expected a connect() error or an EngineEvent::Error with invalid credentials");
    println!("--- observed error: {err:?}");
    assert!(
        err.is_terminal(),
        "invalid credentials must classify as EngineError::Auth (terminal), got: {err:?}"
    );
}

/// SP2 Task 4: `interrupt()` has been a deliberate no-op since S1, pending a
/// live barge-in-during-speech verification — see the doc comment on
/// `NovaEngine::interrupt`. This spike gets the model mid-response, calls
/// `interrupt()`, and records what actually happens on the wire so that
/// comment can state a verified fact instead of an assumption.
#[tokio::test]
#[ignore = "live: requires AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY with Bedrock access and OPENAI_API_KEY to synthesise driving speech"]
async fn interrupt_during_a_live_response_does_not_error_and_stream_keeps_working() {
    use std::time::Duration;
    use uia_core::engine::{EngineEvent, S2sEngine, SessionConfig};

    let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID must be set");
    let secret_access_key =
        std::env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY must be set");
    let openai_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");

    let mut engine = uia_nova::NovaEngine::new(
        uia_nova::NovaEngine::DEFAULT_REGION.into(),
        access_key_id,
        secret_access_key,
        uia_nova::DEFAULT_MODEL_ID.into(),
    );
    // `AudioChunk` carries raw i16 samples - printing it with `{:?}` dumps
    // thousands of numbers per line, so summarise it to a length instead.
    fn describe(ev: &EngineEvent) -> String {
        match ev {
            EngineEvent::AudioChunk(samples) => format!("AudioChunk({} samples)", samples.len()),
            other => format!("{other:?}"),
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    // Nova rejects a session whose first content block isn't SYSTEM-role
    // ("First content must have SYSTEM role") - SessionConfig::default()
    // has no system_prompt, so this needs one explicitly, same as
    // `nova_satisfies_the_engine_contract` does via `assert_engine_contract`.
    let cfg = SessionConfig {
        system_prompt: Some("You are a helpful voice assistant. Answer briefly.".into()),
        ..SessionConfig::default()
    };
    engine.connect(&cfg, tx).await.expect("connect failed");

    // Reuse the same TTS-driven speech technique as
    // `nova_satisfies_the_engine_contract` to get the assistant talking.
    let speech = synthesize_sydney_time_question(&openai_key).await;
    for chunk in speech.chunks(320) {
        engine.send_audio(chunk).await.expect("send_audio failed");
    }

    // Wait until the model is actively streaming audio back. `SpeechStarted`
    // is ambiguous - it also fires for `userSpeechStart` (the VAD detecting
    // our own synthesized speech), so waiting on it caught the user's speech
    // starting, not the assistant's, and interrupt() fired before the model
    // had said anything. `AudioChunk` only ever originates from the
    // assistant's audio, so it's the unambiguous signal that we're mid-response.
    let mut saw_audio_chunk = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            println!("--- pre-interrupt event: {}", describe(&ev));
            if matches!(ev, EngineEvent::AudioChunk(_)) {
                saw_audio_chunk = true;
                break;
            }
        }
    }
    assert!(
        saw_audio_chunk,
        "model never started speaking - can't test interrupt mid-response"
    );

    // Now call interrupt() while it's mid-response and record what happens.
    let interrupt_result = engine.interrupt().await;
    println!("--- interrupt() returned: {interrupt_result:?}");

    // The session must still be usable afterward - drain a few more seconds
    // and confirm no EngineEvent::Closed or unexpected Error follows.
    let mut post_interrupt_events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            post_interrupt_events.push(ev);
        }
    }
    println!(
        "--- post-interrupt events: [{}]",
        post_interrupt_events
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        interrupt_result.is_ok(),
        "interrupt() must not error on a live session"
    );
    assert!(
        !post_interrupt_events
            .iter()
            .any(|e| matches!(e, EngineEvent::Closed)),
        "session closed unexpectedly after interrupt(): {post_interrupt_events:?}"
    );
}
