// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

/// Synthesises "What time is it in Sydney?" via OpenAI TTS — the same
/// technique `uia-app`'s `provider_ab.rs` proved reliably triggers a tool
/// call on both providers (10/10 across S14's A/B), reused here rather than
/// checking in a static fixture so the clip stays byte-reproducible from the
/// prompt text alone.
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
#[ignore = "live: requires OPENAI_API_KEY"]
async fn openai_satisfies_the_engine_contract() {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let speech = synthesize_sydney_time_question(&key).await;
    let engine = uia_openai::OpenAiEngine::new(key, "gpt-realtime-2.1".into());
    uia_engine_contract::assert_engine_contract(Box::new(engine), &speech).await;
}
