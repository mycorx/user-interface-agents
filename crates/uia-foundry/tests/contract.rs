// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

/// Synthesises "What time is it in Sydney?" via OpenAI TTS — same technique
/// `uia-nova`'s and `uia-openai`'s live contract tests use. Foundry's
/// own credentials cannot synthesise speech, so this test needs both
/// providers' keys, matching how `nova_satisfies_the_engine_contract` already
/// needs OpenAI's TTS alongside Bedrock's own credentials.
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
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[tokio::test]
#[ignore = "live: requires AZURE_AI_FOUNDRY_ENDPOINT / AZURE_AI_FOUNDRY_API_KEY / AZURE_AI_FOUNDRY_DEPLOYMENT, and OPENAI_API_KEY to synthesise driving speech"]
async fn foundry_satisfies_the_engine_contract() {
    let endpoint =
        std::env::var("AZURE_AI_FOUNDRY_ENDPOINT").expect("AZURE_AI_FOUNDRY_ENDPOINT must be set");
    let api_key =
        std::env::var("AZURE_AI_FOUNDRY_API_KEY").expect("AZURE_AI_FOUNDRY_API_KEY must be set");
    let deployment = std::env::var("AZURE_AI_FOUNDRY_DEPLOYMENT")
        .expect("AZURE_AI_FOUNDRY_DEPLOYMENT must be set");
    let openai_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");

    let speech = synthesize_sydney_time_question(&openai_key).await;
    let engine = uia_foundry::FoundryEngine::new(endpoint, api_key, deployment);
    uia_engine_contract::assert_engine_contract(Box::new(engine), &speech).await;
}
