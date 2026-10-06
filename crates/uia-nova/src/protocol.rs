// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Wire protocol for Nova Sonic's bidirectional stream, parsed offline.
//!
//! Event names and shapes are exactly what S1 observed live (findings §2.3),
//! not the public docs — `userSpeechStart`/`userSpeechEnd` are undocumented,
//! and `completionEnd` is unreliable (see [`parse_server_event`]'s handling of
//! `contentEnd`). An unrecognised event yields zero `EngineEvent`s, never an
//! error: providers add events, and one must not be able to kill a session.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use uia_core::engine::{EngineError, EngineEvent};
use uia_core::tools::ToolDescriptor;
use uia_mcp::to_nova_declaration;

pub fn session_start_event() -> Value {
    json!({"event": {"sessionStart": {
        "inferenceConfiguration": {"maxTokens": 512, "topP": 0.9, "temperature": 0.7},
        "turnDetectionConfiguration": {"endpointingSensitivity": "MEDIUM"}
    }}})
}

/// Tool schemas are declared once here as Nova's stringified `inputSchema.json`
/// (`to_nova_declaration`) — contrast OpenAI's per-turn object schema.
/// voices available for Nova 2: https://docs.aws.amazon.com/nova/latest/nova2-userguide/sonic-language-support.html
/// "tiffany:,"amy","olivia","kiara"
pub fn prompt_start_event(
    prompt_name: &str,
    tools: &[ToolDescriptor],
    output_rate_hz: u32,
) -> Value {
    json!({"event": {"promptStart": {
        "promptName": prompt_name,
        "textOutputConfiguration": {"mediaType": "text/plain"},
        "audioOutputConfiguration": {
            "mediaType": "audio/lpcm",
            "sampleRateHertz": output_rate_hz,
            "sampleSizeBits": 16,
            "channelCount": 1,
            "voiceId": "tiffany",
            "encoding": "base64",
            "audioType": "SPEECH"
        },
        "toolUseOutputConfiguration": {"mediaType": "application/json"},
        "toolConfiguration": {
            "tools": tools
                .iter()
                .map(|t| {
                    // Nova enforces `^[a-zA-Z0-9_-]+$` on tool names exactly as
                    // OpenAI does — proven live in S14 by a 400 that named the
                    // pattern. Declared under the wire name; `restore_tool_names`
                    // maps it back for the executor.
                    let mut d = ToolDescriptor {
                        name: uia_mcp::wire_tool_name(&t.name),
                        ..t.clone()
                    };
                    d.description = t.description.clone();
                    to_nova_declaration(&d)
                })
                .collect::<Vec<_>>()
        }
    }}})
}

/// `interactive` distinguishes the two TEXT content blocks this protocol
/// carries. The SYSTEM prompt at connect time is `false` — it is setup, not a
/// turn, and must never make the model answer. A typed user turn is `true`
/// (PLAN.md FD8): that is what tells Nova the block is a live conversational
/// turn to generate off once it closes. Shared deliberately rather than
/// duplicated — the two differ in exactly this one field.
pub fn content_start_text_event(
    prompt_name: &str,
    content_name: &str,
    role: &str,
    interactive: bool,
) -> Value {
    json!({"event": {"contentStart": {
        "promptName": prompt_name, "contentName": content_name, "type": "TEXT",
        "interactive": interactive, "role": role,
        "textInputConfiguration": {"mediaType": "text/plain"}
    }}})
}

pub fn text_input_event(prompt_name: &str, content_name: &str, content: &str) -> Value {
    json!({"event": {"textInput": {
        "promptName": prompt_name, "contentName": content_name, "content": content
    }}})
}

pub fn content_end_event(prompt_name: &str, content_name: &str) -> Value {
    json!({"event": {"contentEnd": {
        "promptName": prompt_name, "contentName": content_name
    }}})
}

/// `interactive: true` — this content block stays open for the whole session;
/// turns are the model's own VAD (`userSpeechStart`/`userSpeechEnd`), not a
/// per-turn re-open of the input stream.
pub fn content_start_audio_event(prompt_name: &str, content_name: &str, rate_hz: u32) -> Value {
    json!({"event": {"contentStart": {
        "promptName": prompt_name, "contentName": content_name, "type": "AUDIO",
        "interactive": true, "role": "USER",
        "audioInputConfiguration": {
            "mediaType": "audio/lpcm", "sampleRateHertz": rate_hz, "sampleSizeBits": 16,
            "channelCount": 1, "audioType": "SPEECH", "encoding": "base64"
        }
    }}})
}

pub fn audio_input_event(prompt_name: &str, content_name: &str, base64_pcm: &str) -> Value {
    json!({"event": {"audioInput": {
        "promptName": prompt_name, "contentName": content_name, "content": base64_pcm
    }}})
}

/// `mediaType` MUST be `text/plain` — the service enumerates the allowed set
/// (`Tool result media type must be one of: [text/plain]`) and rejects
/// `application/json` outright, even though the *content* it carries has to be
/// JSON. Both halves were established live in S14 by making each mistake in
/// turn; neither is guessable from the shape of the request.
pub fn content_start_tool_event(prompt_name: &str, content_name: &str, tool_use_id: &str) -> Value {
    json!({"event": {"contentStart": {
        "promptName": prompt_name, "contentName": content_name, "interactive": false,
        "type": "TOOL", "role": "TOOL",
        "toolResultInputConfiguration": {
            "toolUseId": tool_use_id, "type": "TEXT",
            "textInputConfiguration": {"mediaType": "text/plain"}
        }
    }}})
}

/// The result is wrapped as a JSON document and stringified, the same asymmetry
/// `to_nova_declaration` uses for the schema and `toolUse` uses for arguments.
///
/// S10 sent the tool's text through bare, and nothing caught it: the contract
/// suite declares no tools, so no stage before S14 completed a tool round trip
/// against the real service. Live, a bare sentence gets
/// `ValidationException: Tool Response parsing error` and the session dies —
/// Nova parses the content as JSON despite the `text/plain` media type it
/// insists on above.
pub fn tool_result_event(prompt_name: &str, content_name: &str, content: &str) -> Value {
    json!({"event": {"toolResult": {
        "promptName": prompt_name,
        "contentName": content_name,
        "content": json!({"result": content}).to_string()
    }}})
}

pub fn prompt_end_event(prompt_name: &str) -> Value {
    json!({"event": {"promptEnd": {"promptName": prompt_name}}})
}

pub fn session_end_event() -> Value {
    json!({"event": {"sessionEnd": {}}})
}

/// Encode PCM16 mono samples the way `audioInput`/`audioOutput` carry them:
/// little-endian bytes, base64 text.
pub fn encode_pcm16(samples: &[i16]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    BASE64.encode(bytes)
}

fn decode_pcm16(base64_pcm: &str) -> Result<Vec<i16>, EngineError> {
    let bytes = BASE64
        .decode(base64_pcm)
        .map_err(|e| EngineError::Protocol(e.to_string()))?;
    Ok(bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect())
}

/// Parse one decoded response chunk off the bidirectional stream.
///
/// `Ok(vec![])` means "recognised but nothing to report", or "not recognised
/// at all" — deliberately indistinguishable to the caller, same contract as
/// `uia-openai`'s parser. Only malformed JSON or malformed base64 audio is
/// an error.
pub fn parse_server_event(raw: &str) -> Result<Vec<EngineEvent>, EngineError> {
    let v: Value = serde_json::from_str(raw).map_err(|e| EngineError::Protocol(e.to_string()))?;
    let Some(event) = v.get("event").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let Some((kind, body)) = event.iter().next() else {
        return Ok(Vec::new());
    };

    let ev = match kind.as_str() {
        // Undocumented, observed live (findings §2.3) — the barge-in trigger.
        "userSpeechStart" => EngineEvent::SpeechStarted,
        "userSpeechEnd" => EngineEvent::SpeechEnded,

        // The assistant's audio bracket opening/closing mirrors OpenAI's
        // `output_audio_buffer.started` / `response.output_audio.done`.
        "contentStart" if body.get("type").and_then(Value::as_str) == Some("AUDIO") => {
            EngineEvent::SpeechStarted
        }
        // The turn boundary to act on (PLAN.md, from findings §2.3):
        // `completionEnd` does not arrive until the client closes the prompt,
        // which would deadlock a continuous conversation.
        "contentEnd"
            if body.get("type").and_then(Value::as_str) == Some("AUDIO")
                && body.get("stopReason").and_then(Value::as_str) == Some("END_TURN") =>
        {
            // Both boundaries at once, unlike OpenAI. Nova has no separate
            // transcript-done event to wait for: `textOutput` carries whole
            // utterances and has already arrived by the time this does, so
            // this single event is the playback bracket closing *and* the
            // point at which the exchange is complete. Emitting `TurnComplete`
            // here keeps Nova recording exactly what it recorded when the
            // session flushed on `SpeechEnded`, while OpenAI and Foundry move
            // to their own later boundary.
            return Ok(vec![EngineEvent::SpeechEnded, EngineEvent::TurnComplete]);
        }

        // Nova emits whole utterances, not deltas, so `textOutput` maps
        // straight to a transcript by its `role` — no separate .done event
        // exists to gate on, unlike OpenAI's delta/done pair.
        "textOutput" => {
            let content = body
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match body.get("role").and_then(Value::as_str) {
                Some("USER") => EngineEvent::UserTranscript(content),
                Some("ASSISTANT") => EngineEvent::ModelTranscript(content),
                _ => return Ok(Vec::new()),
            }
        }

        "toolUse" => EngineEvent::ToolCall {
            id: body
                .get("toolUseId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: body
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            // `content` is a STRING holding JSON args, same asymmetry as the
            // stringified schema in `to_nova_declaration`.
            args: body
                .get("content")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null),
        },

        "audioOutput" => {
            let b64 = body.get("content").and_then(Value::as_str).unwrap_or("");
            EngineEvent::AudioChunk(decode_pcm16(b64)?)
        }

        _ => return Ok(Vec::new()),
    };
    Ok(vec![ev])
}

/// Map every `ToolCall` back from its wire name to the name the executor is
/// keyed by. Applied by the engine's reader task, which is the only place that
/// knows which tools this session declared.
pub fn restore_tool_names(
    events: Vec<EngineEvent>,
    names: &uia_mcp::ToolNames,
) -> Vec<EngineEvent> {
    events
        .into_iter()
        .map(|e| match e {
            EngineEvent::ToolCall { id, name, args } => EngineEvent::ToolCall {
                id,
                name: names.restore(&name),
                args,
            },
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_prompt_content_block_is_not_interactive() {
        // Regression guard: `content_start_text_event` is shared with the
        // typed-turn path as of S1, and `false` is still correct here — the
        // system prompt is setup, not a turn to answer (PLAN.md FD8).
        let e = content_start_text_event("p", "system-prompt", "SYSTEM", false);
        let start = &e["event"]["contentStart"];
        assert_eq!(start["interactive"], false);
        assert_eq!(start["role"], "SYSTEM");
        assert_eq!(start["type"], "TEXT");
        assert_eq!(start["textInputConfiguration"]["mediaType"], "text/plain");
    }

    #[test]
    fn a_typed_user_turn_content_block_is_interactive() {
        let e = content_start_text_event("p", "user-text-1", "USER", true);
        let start = &e["event"]["contentStart"];
        assert_eq!(start["interactive"], true);
        assert_eq!(start["role"], "USER");
        assert_eq!(start["type"], "TEXT");
        assert_eq!(start["promptName"], "p");
        assert_eq!(start["contentName"], "user-text-1");
        assert_eq!(start["textInputConfiguration"]["mediaType"], "text/plain");
    }

    #[test]
    fn user_speech_start_parses_to_speech_started() {
        let raw = r#"{"event":{"userSpeechStart":{"inputAudioOffsetMs":0}}}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::SpeechStarted]
        ));
    }

    #[test]
    fn user_speech_end_parses_to_speech_ended() {
        let raw = r#"{"event":{"userSpeechEnd":{"inputAudioOffsetMs":500}}}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::SpeechEnded]
        ));
    }

    #[test]
    fn the_assistant_audio_bracket_opening_parses_to_speech_started() {
        let raw = r#"{"event":{"contentStart":{"type":"AUDIO",
                      "audioOutputConfiguration":{"sampleRateHertz":24000}}}}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::SpeechStarted]
        ));
    }

    #[test]
    fn a_non_audio_content_start_yields_no_events() {
        // The ASR/preview text brackets carry their payload on `textOutput`;
        // the bracketing `contentStart` itself is not event-worthy.
        let raw = r#"{"event":{"contentStart":{"type":"TEXT","role":"USER"}}}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn the_audio_content_end_with_end_turn_ends_both_the_speech_and_the_turn() {
        // Nova carries both boundaries on one event, because `textOutput`
        // delivers whole utterances that have already arrived by now — there
        // is no later transcript-done to wait for as there is on OpenAI. The
        // session records the exchange on `TurnComplete`, so without it here
        // Nova would stop recording entirely.
        let raw = r#"{"event":{"contentEnd":{"type":"AUDIO","stopReason":"END_TURN"}}}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::SpeechEnded, EngineEvent::TurnComplete]
        ));
    }

    #[test]
    fn a_tool_content_end_yields_no_events() {
        // The tool round trip is already reported by `toolUse`; this bracket
        // close (`stopReason: TOOL_USE`) must not double it.
        let raw = r#"{"event":{"contentEnd":{"type":"TOOL","stopReason":"TOOL_USE"}}}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn a_user_text_output_parses_to_a_user_transcript() {
        let raw = r#"{"event":{"textOutput":{"content":"what time is it in sydney?",
                      "role":"USER"}}}"#;
        match &parse_server_event(raw).unwrap()[0] {
            EngineEvent::UserTranscript(t) => assert_eq!(t, "what time is it in sydney?"),
            other => panic!("expected UserTranscript, got {other:?}"),
        }
    }

    #[test]
    fn an_assistant_text_output_parses_to_one_model_transcript() {
        // Nova emits the whole utterance in one event — no delta/final split,
        // so no filtering is needed to keep this at one-per-turn.
        let raw = r#"{"event":{"textOutput":{"content":"The current time in Sydney is 00:54.",
                      "role":"ASSISTANT"}}}"#;
        match &parse_server_event(raw).unwrap()[0] {
            EngineEvent::ModelTranscript(t) => {
                assert_eq!(t, "The current time in Sydney is 00:54.")
            }
            other => panic!("expected ModelTranscript, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_use_parses_with_id_name_and_decoded_args() {
        let raw = r#"{"event":{"toolUse":{"toolName":"get_time",
                      "toolUseId":"b67420c3-dc6a-4502-8f2b-cd457741436d",
                      "content":"{\"city\":\"sydney\"}"}}}"#;
        match &parse_server_event(raw).unwrap()[0] {
            EngineEvent::ToolCall { id, name, args } => {
                assert_eq!(id, "b67420c3-dc6a-4502-8f2b-cd457741436d");
                assert_eq!(name, "get_time");
                assert_eq!(args["city"], "sydney");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn audio_output_decodes_base64_lpcm_into_i16_samples() {
        let samples: Vec<i16> = vec![1, -1, 12345, i16::MIN, i16::MAX];
        let b64 = encode_pcm16(&samples);
        let raw = format!(r#"{{"event":{{"audioOutput":{{"content":"{b64}"}}}}}}"#);
        match &parse_server_event(&raw).unwrap()[0] {
            EngineEvent::AudioChunk(chunk) => assert_eq!(chunk, &samples),
            other => panic!("expected AudioChunk, got {other:?}"),
        }
    }

    #[test]
    fn malformed_base64_audio_is_a_protocol_error_not_a_panic() {
        let raw = r#"{"event":{"audioOutput":{"content":"not-valid-base64!!"}}}"#;
        assert!(matches!(
            parse_server_event(raw),
            Err(EngineError::Protocol(_))
        ));
    }

    #[test]
    fn completion_end_yields_no_events() {
        // Never act on this — it does not arrive until the client closes the
        // prompt, and waiting for it deadlocks a continuous conversation.
        let raw = r#"{"event":{"completionEnd":{"stopReason":"END_TURN"}}}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn usage_events_yield_no_events() {
        let raw = r#"{"event":{"usageEvent":{"totalTokens":42}}}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn an_unknown_event_yields_no_events_rather_than_an_error() {
        let raw = r#"{"event":{"somethingFutureAddsLater":{"x":1}}}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn a_response_with_no_event_key_yields_no_events() {
        let raw = r#"{"notAnEvent":true}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn malformed_json_is_a_protocol_error() {
        assert!(matches!(
            parse_server_event("not json at all"),
            Err(EngineError::Protocol(_))
        ));
    }

    #[test]
    fn prompt_start_declares_tools_via_the_nova_stringified_schema() {
        let tool = ToolDescriptor {
            name: "clock.now".into(),
            description: "Current time".into(),
            input_schema: serde_json::json!({"type": "object"}),
            requires_confirmation: false,
        };
        let ev = prompt_start_event("p1", std::slice::from_ref(&tool), 24_000);
        let tools = &ev["event"]["promptStart"]["toolConfiguration"]["tools"];
        // Was `clock.now` until S14 sent one live: Nova returns a 400 naming
        // the same `^[a-zA-Z0-9_-]+$` pattern OpenAI enforces. This assertion
        // encoded PLAN.md's "Nova accepts dots", which was never tested against
        // the service and is wrong.
        assert_eq!(tools[0]["toolSpec"]["name"], "clock__now");
        assert!(tools[0]["toolSpec"]["inputSchema"]["json"].is_string());
        assert_eq!(
            ev["event"]["promptStart"]["audioOutputConfiguration"]["sampleRateHertz"],
            24_000
        );
    }

    #[test]
    fn content_start_audio_declares_the_requested_rate_and_stays_interactive() {
        let ev = content_start_audio_event("p1", "user-audio", 16_000);
        let cs = &ev["event"]["contentStart"];
        assert_eq!(cs["audioInputConfiguration"]["sampleRateHertz"], 16_000);
        // Interactive: the block stays open for the whole session; turns are
        // the model's own VAD, not a per-turn re-open of the input stream.
        assert_eq!(cs["interactive"], true);
    }

    #[test]
    fn audio_input_round_trips_through_encode_and_decode() {
        let samples: Vec<i16> = vec![0, 100, -100, i16::MAX, i16::MIN];
        let b64 = encode_pcm16(&samples);
        let ev = audio_input_event("p1", "user-audio", &b64);
        let decoded = decode_pcm16(ev["event"]["audioInput"]["content"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, samples);
    }
}

#[cfg(test)]
mod s14_tool_name_tests {
    use super::*;
    use uia_core::tools::ToolDescriptor;

    fn clock_tool() -> ToolDescriptor {
        ToolDescriptor {
            name: "clock.now".into(),
            description: "Get the current local time.".into(),
            input_schema: json!({"type": "object"}),
            requires_confirmation: false,
        }
    }

    #[test]
    fn a_namespaced_tool_is_declared_with_the_dot_translated_away() {
        // Live, S14: Nova returns a 400 and NO session at all for a dotted
        // name — `Malformed input request: #/toolConfig/tools/0/toolSpec/name:
        // string [clock.now] does not match pattern ^[a-zA-Z0-9_-]+$`.
        // PLAN.md said Nova accepted dots. It does not, and the consequence is
        // worse than OpenAI's: OpenAI drops the tool declaration, Nova refuses
        // to open the session, so a single MCP tool made the whole fallback
        // engine unusable.
        let ev = prompt_start_event("p", &[clock_tool()], 24_000);
        let name = &ev["event"]["promptStart"]["toolConfiguration"]["tools"][0]["toolSpec"]["name"];
        assert_eq!(name, "clock__now");
    }

    #[test]
    fn a_tool_call_comes_back_under_the_name_the_executor_is_keyed_by() {
        let names = uia_mcp::ToolNames::new(&[clock_tool()]);
        let raw = r#"{"event":{"toolUse":{"toolName":"clock__now",
                      "toolUseId":"abc","content":"{\"city\":\"Tokyo\"}"}}}"#;
        let events = parse_server_event(raw).unwrap();
        let restored = restore_tool_names(events, &names);
        match &restored[0] {
            EngineEvent::ToolCall { name, args, .. } => {
                assert_eq!(
                    name, "clock.now",
                    "the router dispatches on the dotted name"
                );
                assert_eq!(args["city"], "Tokyo");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }
}
