// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Wire protocol for OpenAI Realtime, parsed offline.
//!
//! Every event name here is the **GA** vocabulary measured live in S1
//! (`docs/superpowers/findings/2026-08-16-provider-formats.md` §3.4), not the
//! beta names the task plan guessed. Two structural rules hold:
//!
//! 1. **No audio arrives here.** Over WebRTC the audio bytes are RTP media on
//!    the track; the `oai-events` data channel carries metadata only. A parser
//!    branch that decodes base64 audio from an event is wrong by construction.
//! 2. **An unrecognised event yields zero events, never an error.** Providers
//!    add events; one must not be able to kill a session.

use serde_json::{Value, json};
use uia_core::engine::{EngineError, EngineEvent, SessionConfig, TextTurnSupport};
use uia_core::tools::ToolDescriptor;
use uia_mcp::to_openai_declaration;

/// Parse one server event off the `oai-events` data channel.
///
/// `Ok(vec![])` means "recognised but nothing to report", or "not recognised at
/// all" — deliberately indistinguishable to the caller. Only malformed JSON is
/// an error.
pub fn parse_server_event(raw: &str) -> Result<Vec<EngineEvent>, EngineError> {
    let v: Value = serde_json::from_str(raw).map_err(|e| EngineError::Protocol(e.to_string()))?;
    let Some(kind) = v.get("type").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    let ev = match kind {
        "session.created" => EngineEvent::Ready,

        // The assistant's playback bracket opens. `.stopped` deliberately maps
        // to nothing: it trails `response.output_audio.done`, and emitting both
        // would report two turn endings for one turn.
        "output_audio_buffer.started" => EngineEvent::SpeechStarted,
        "response.output_audio.done" => EngineEvent::SpeechEnded,

        // The turn is over, transcripts included. Separate from `SpeechEnded`
        // above because `response.output_audio.done` does NOT trail the
        // transcripts: it reports the audio finishing, and
        // `response.output_audio_transcript.done` can follow it. Recording an
        // exchange at the audio boundary therefore caught whichever halves had
        // arrived and spilled the rest into the next exchange. Observed live:
        // one spoken sentence produced a user-only entry followed by an
        // assistant-only one.
        "response.done" => EngineEvent::TurnComplete,

        // One transcript per turn, on `.done`. Deltas are dropped: `EngineEvent`
        // has no delta/final distinction, Nova emits whole utterances, and S11
        // runs ONE contract suite against both engines.
        "response.output_audio_transcript.done" => EngineEvent::ModelTranscript(
            v.get("transcript")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),

        // What the user said, transcribed by `whisper-1`. Observed live in S16
        // (S9 could send no audio, so it never saw this): the name is the long
        // `conversation.item.*` form, NOT `input_audio_transcription.completed`,
        // and the text is in `transcript`. Its `.delta` twin is dropped for the
        // same reason the model transcript's is -- one event per utterance.
        "conversation.item.input_audio_transcription.completed" => EngineEvent::UserTranscript(
            v.get("transcript")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),

        "response.function_call_arguments.done" => EngineEvent::ToolCall {
            // `call_id` is what a function_call_output must reference — NOT `item_id`.
            id: v
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            // `arguments` is a STRING holding JSON. A model can emit malformed
            // args; that is the tool's problem to report, not a dead session.
            args: v
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null),
        },

        "error" => {
            let err = v.get("error").unwrap_or(&Value::Null);
            if is_benign(err) {
                return Ok(Vec::new());
            }
            EngineEvent::Error(engine_error(err))
        }

        _ => return Ok(Vec::new()),
    };
    Ok(vec![ev])
}

/// Errors that report a no-op rather than a fault, and must not reach the
/// session: reporting one would classify a healthy connection as
/// transient-failed and reconnect it.
fn is_benign(err: &Value) -> bool {
    // Barge-in races the end of a response, so a late `response.cancel` finding
    // nothing to cancel is expected traffic, not a failure. Verified live.
    err.get("code").and_then(Value::as_str) == Some("response_cancel_not_active")
}

/// Classify a server error. Auth failures are terminal and must never enter the
/// backoff loop; everything else is transient.
fn engine_error(err: &Value) -> EngineError {
    let field = |k: &str| {
        err.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let message = match field("message").as_str() {
        "" => err.to_string(),
        m => m.to_string(),
    };
    let code = format!("{} {}", field("code"), field("type")).to_lowercase();
    if ["api_key", "auth", "unauthorized", "forbidden", "permission"]
        .iter()
        .any(|needle| code.contains(needle))
    {
        EngineError::Auth(message)
    } else {
        EngineError::Transport(message)
    }
}

/// OpenAI enforces `^[a-zA-Z0-9_-]+$` on tool names, and rejects the whole
/// `session.update` if one violates it. S8's `namespaced()` builds `server.tool`,
/// so every namespaced tool is refused until the dot is translated away.
///
/// `.` becomes `__` rather than `_` so `a.b` and `a_b` stay distinct.
pub fn openai_tool_name(name: &str) -> String {
    name.replace('.', "__")
}

/// The inverse map, built from the tools actually declared this session.
///
/// Inverting by string rewrite would be ambiguous; the executor is keyed by the
/// original namespaced name, and calling the wrong server is worse than failing.
#[derive(Debug, Default, Clone)]
pub struct ToolNames(std::collections::HashMap<String, String>);

impl ToolNames {
    pub fn new(tools: &[ToolDescriptor]) -> Self {
        Self(
            tools
                .iter()
                .map(|t| (openai_tool_name(&t.name), t.name.clone()))
                .collect(),
        )
    }

    /// Map a name the model used back to the declared one. An undeclared name
    /// passes through untouched so the executor reports "not found" rather than
    /// this layer guessing.
    pub fn restore(&self, wire_name: &str) -> String {
        self.0
            .get(wire_name)
            .cloned()
            .unwrap_or_else(|| wire_name.to_string())
    }
}

/// The `session.update` sent once the data channel opens.
///
/// Tools are declared as objects (`to_openai_declaration`) — contrast Nova's
/// stringified schema. Transcription is opt-in: it costs latency and money, and
/// in SP1 nothing consumes it.
pub fn session_update(cfg: &SessionConfig) -> Value {
    let mut session = json!({
        "type": "realtime",
        "tool_choice": "auto",
        "tools": cfg.tools.iter().map(|t| {
            let mut d = to_openai_declaration(t);
            d["type"] = json!("function");
            // S8 names tools `server.tool`; OpenAI refuses the dot.
            d["name"] = json!(openai_tool_name(&t.name));
            d
        }).collect::<Vec<_>>(),
    });

    if let Some(prompt) = &cfg.system_prompt {
        session["instructions"] = json!(prompt);
    }
    // GA `gpt-realtime*` schema nests both directions under `session.audio`
    // (findings §3.2: a live session dump came back with
    // `audio.output.voice`, not a top-level `session.voice` — that was the
    // beta-era field location). Both branches write into the same `audio`
    // object rather than each assigning `session["audio"]` outright, or
    // whichever ran second would silently erase the other's setting.
    if cfg.voice.is_some() || cfg.transcription {
        let mut audio = json!({});
        if let Some(voice) = &cfg.voice {
            audio["output"] = json!({"voice": voice});
        }
        if cfg.transcription {
            audio["input"] = json!({"transcription": {"model": TRANSCRIPTION_MODEL}});
        }
        session["audio"] = audio;
    }

    json!({"type": "session.update", "session": session})
}

/// UNVERIFIED against a live session — S1 sent no microphone audio, so no
/// input-transcription event was ever observed. Confirm before relying on it.
const TRANSCRIPTION_MODEL: &str = "whisper-1";

/// A user turn submitted as text rather than audio (PLAN.md S4), using the
/// same `conversation.item.create` shape `send_tool_result` already sends a
/// `function_call_output` item through — the GA schema OpenAI and Foundry
/// share byte-for-byte. The caller still has to follow this with a
/// `response.create`, exactly as `send_tool_result` does: creating the item
/// alone does not make the model speak to it.
pub fn text_turn_event(text: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        }
    })
}

/// Prior conversation replayed into a new session, oldest first.
///
/// Recall was already being fetched and put on `SessionConfig.memory_context`;
/// nothing read it. The session paid a recall deadline on every connect and
/// dropped the result, so "replays recent exchanges when the session
/// reconnects" was never true on any engine.
///
/// Sent as real conversation items rather than pasted into the instructions:
/// the model then sees prior turns as turns, attributed to whoever said them,
/// instead of a wall of quoted text inside its system prompt.
///
/// The content types are not interchangeable and are not symmetrical: a user
/// item carries `input_text`, an assistant item carries `output_text`. Sending
/// `text` for the assistant is rejected outright with "Invalid value: 'text'.
/// Value must be 'output_text'", once per recalled line, on every connect.
/// Anything whose role tag is unrecognised is skipped rather than guessed at:
/// replaying a line under the wrong speaker teaches the model the user said
/// something it did not.
pub fn memory_item_events(cfg: &SessionConfig) -> Vec<Value> {
    cfg.memory_context
        .iter()
        .filter_map(|item| {
            let uia_core::memory::MemoryKind::Other(role) = &item.kind else {
                return None;
            };
            let (role, content_type) = match role.as_str() {
                uia_core::memory::RECALL_ROLE_USER => ("user", "input_text"),
                uia_core::memory::RECALL_ROLE_ASSISTANT => ("assistant", "output_text"),
                _ => return None,
            };
            Some(json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "message",
                    "role": role,
                    "content": [{"type": content_type, "text": item.content}],
                }
            }))
        })
        .collect()
}

/// The handshake's capability answer, shared between an engine and the read
/// task that will discover it.
///
/// `text_turn_support()` is a synchronous `&self` read on the trait while
/// `session.created` arrives later, inside the spawned read task — so the
/// answer needs interior mutability shared across that boundary (S0's ledger
/// note flagged exactly this). It lives here rather than in either engine
/// because Foundry is a transport wrapper around this same protocol: one
/// implementation, used twice, is the whole point of FD4's "no duplicated
/// parsing".
#[derive(Clone, Debug, Default)]
pub struct HandshakeCapability(std::sync::Arc<std::sync::Mutex<Option<TextTurnSupport>>>);

impl HandshakeCapability {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the previous connection's answer. A reconnect — especially one
    /// that follows a deployment change — must not inherit a claim the new
    /// session never made.
    pub fn reset(&self) {
        *self.lock() = None;
    }

    /// Record whatever this raw server event says about typed turns. Events
    /// that say nothing leave the stored answer alone.
    pub fn observe(&self, raw: &str) {
        if let Some(support) = text_turn_support_from_event(raw) {
            *self.lock() = Some(support);
        }
    }

    /// The handshake's answer, or `fallback` when the handshake made no
    /// claim.
    ///
    /// A parsed `Unknown` is "we could not tell", which is exactly what the
    /// engine's own static default already says — so a fallback of
    /// `Supported` (OpenAI, proven end to end in S0) is deliberately NOT
    /// overwritten by one. Only a positive `Supported`/`Unsupported` from the
    /// wire replaces it.
    pub fn get_or(&self, fallback: TextTurnSupport) -> TextTurnSupport {
        match self.lock().clone() {
            Some(TextTurnSupport::Unknown) | None => fallback,
            Some(answer) => answer,
        }
    }

    /// A poisoned lock still holds a perfectly good answer: a panic elsewhere
    /// must not cost the user their input (FD3).
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<TextTurnSupport>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Derive text-turn capability from one raw server event, or `None` if this
/// event says nothing about it.
///
/// `None` is not `Unknown`: every non-`session.created` event on the channel
/// (and anything that isn't JSON) simply isn't a capability signal, and an
/// engine holding a real handshake answer must not have it overwritten by the
/// next transcript that goes past. Only a `session.created` produces an
/// answer, and that answer may itself be `Unknown` (FD5).
pub fn text_turn_support_from_event(raw: &str) -> Option<TextTurnSupport> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("session.created") {
        return None;
    }
    Some(text_turn_support_from_session_created(&v))
}

/// The modalities the server echoed back at handshake, read as a capability
/// (FD4 — the authoritative, per-deployment answer; never a model-name
/// allowlist, which for an Azure deployment name would be a guess at a string
/// the user typed in their own portal).
///
/// Tolerant by construction (FD5). Realtime API versions disagree about the
/// field: the beta-era `modalities` listed what the session could do in both
/// directions, while GA split output off into `output_modalities`. That split
/// is why the two fields resolve differently when text is *missing*:
///
/// - `modalities` without `text` is a positive statement that this session has
///   no text channel at all → `Unsupported`.
/// - `output_modalities` without `text` only says the model answers in audio.
///   A GA `gpt-realtime` session is audio-out by default and still accepts
///   typed input — that is exactly the path S0 proved end to end — so reading
///   it as `Unsupported` would disable the control on the one engine known to
///   work. It says nothing about input → `Unknown`.
///
/// Anything else — field absent, not an array, not strings, empty — is "we
/// could not tell", which by FD3 leaves the user their input.
pub fn text_turn_support_from_session_created(event: &Value) -> TextTurnSupport {
    let session = event.get("session").unwrap_or(&Value::Null);

    if let Some(modalities) = modality_list(session.get("modalities")) {
        return if lists_text(&modalities) {
            TextTurnSupport::Supported
        } else {
            TextTurnSupport::Unsupported(format!(
                "This session was created with {} only, so it has no text channel \u{2014} speak instead.",
                modalities.join(" and ")
            ))
        };
    }

    if let Some(output) = modality_list(session.get("output_modalities")) {
        if lists_text(&output) {
            return TextTurnSupport::Supported;
        }
        // Audio-only OUTPUT, which is the GA default and says nothing about
        // whether typed input is accepted.
        return TextTurnSupport::Unknown;
    }

    TextTurnSupport::Unknown
}

/// A modalities field read as a list, or `None` for "that isn't a list of
/// modalities" — including an empty one, which claims nothing.
fn modality_list(field: Option<&Value>) -> Option<Vec<String>> {
    let items = field?.as_array()?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(item.as_str()?.trim().to_ascii_lowercase());
    }
    (!out.is_empty()).then_some(out)
}

fn lists_text(modalities: &[String]) -> bool {
    modalities.iter().any(|m| m == "text")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uia_core::engine::{EngineError, EngineEvent, SessionConfig, TextTurnSupport};
    use uia_core::tools::ToolDescriptor;

    /// Every capability assertion below goes through the raw-event entry
    /// point, because that is what the engine's read task actually calls.
    fn support(raw: &str) -> Option<TextTurnSupport> {
        text_turn_support_from_event(raw)
    }

    #[test]
    fn modalities_including_text_are_supported() {
        // The beta-era combined field, listing both directions.
        let raw = r#"{"type":"session.created","session":{"modalities":["audio","text"]}}"#;
        assert_eq!(support(raw), Some(TextTurnSupport::Supported));
    }

    #[test]
    fn output_modalities_including_text_are_supported() {
        // Same claim, GA field name (FD5's version skew).
        let raw = r#"{"type":"session.created","session":{"output_modalities":["text"]}}"#;
        assert_eq!(support(raw), Some(TextTurnSupport::Supported));
    }

    #[test]
    fn modalities_excluding_text_are_unsupported_with_a_readable_reason() {
        // A positive statement that this session has no text channel at all —
        // the ONE shape that may disable the control (FD5).
        let raw = r#"{"type":"session.created","session":{"modalities":["audio"]}}"#;
        let got = support(raw).expect("session.created always answers");
        assert!(!got.is_usable());
        let reason = got.reason().expect("Unsupported carries its reason");
        assert!(
            reason.contains("audio") && reason.ends_with("speak instead."),
            "the reason is shown verbatim next to the disabled control (FD6): {reason}"
        );
    }

    #[test]
    fn audio_only_output_modalities_are_unknown_not_unsupported() {
        // The regression this parser exists to avoid: a GA `gpt-realtime`
        // session is audio-OUT by default and still takes typed input — the
        // path S0 proved end to end. Reading it as `Unsupported` would
        // disable the control on the one engine known to work.
        let raw = r#"{"type":"session.created","session":{"output_modalities":["audio"]}}"#;
        assert_eq!(support(raw), Some(TextTurnSupport::Unknown));
    }

    #[test]
    fn an_absent_modalities_field_is_unknown() {
        // FD5: absence is "we could not tell", never "no".
        let raw = r#"{"type":"session.created","session":{"type":"realtime"}}"#;
        assert_eq!(support(raw), Some(TextTurnSupport::Unknown));
        let no_session = r#"{"type":"session.created"}"#;
        assert_eq!(support(no_session), Some(TextTurnSupport::Unknown));
    }

    #[test]
    fn a_malformed_modalities_payload_is_unknown() {
        // Wrong type, wrong element type, and empty — none of them claim
        // anything, so none of them may cost the user their input (FD3).
        for raw in [
            r#"{"type":"session.created","session":{"modalities":"text"}}"#,
            r#"{"type":"session.created","session":{"modalities":[{"kind":"audio"}]}}"#,
            r#"{"type":"session.created","session":{"modalities":[]}}"#,
            r#"{"type":"session.created","session":42}"#,
        ] {
            assert_eq!(support(raw), Some(TextTurnSupport::Unknown), "{raw}");
        }
    }

    #[test]
    fn only_session_created_answers_at_all() {
        // `None` is not `Unknown`: an engine holding a real handshake answer
        // must not have it erased by the next transcript on the channel.
        assert_eq!(
            support(r#"{"type":"response.output_audio.done"}"#),
            None,
            "an unrelated event is not a capability signal"
        );
        assert_eq!(support("not json at all"), None);
        assert_eq!(support(r#"{"session":{"modalities":["text"]}}"#), None);
    }

    #[test]
    fn the_capability_parser_reads_the_same_event_that_reports_ready() {
        // One event, two independent readings — the read task runs both, so a
        // capability answer must never cost the session its `Ready`.
        let raw = r#"{"type":"session.created","session":{"modalities":["audio","text"]}}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::Ready]
        ));
        assert_eq!(support(raw), Some(TextTurnSupport::Supported));
    }

    #[test]
    fn the_handshake_answer_overrides_the_engines_static_default() {
        let cap = HandshakeCapability::new();
        assert_eq!(
            cap.get_or(TextTurnSupport::Supported),
            TextTurnSupport::Supported,
            "before the handshake, the engine's own default stands"
        );
        cap.observe(r#"{"type":"session.created","session":{"modalities":["audio"]}}"#);
        assert!(!cap.get_or(TextTurnSupport::Supported).is_usable());
    }

    #[test]
    fn a_parsed_unknown_never_downgrades_a_proven_default() {
        // FD5: "we could not tell" is what the static default already says.
        // OpenAI's `Supported` is a measured fact (S0), not a guess, so an
        // uninformative handshake must not talk it down.
        let cap = HandshakeCapability::new();
        cap.observe(r#"{"type":"session.created","session":{"output_modalities":["audio"]}}"#);
        assert_eq!(
            cap.get_or(TextTurnSupport::Supported),
            TextTurnSupport::Supported
        );
        assert_eq!(
            cap.get_or(TextTurnSupport::Unknown),
            TextTurnSupport::Unknown
        );
    }

    #[test]
    fn later_events_do_not_erase_the_handshake_answer() {
        let cap = HandshakeCapability::new();
        cap.observe(r#"{"type":"session.created","session":{"modalities":["audio","text"]}}"#);
        cap.observe(r#"{"type":"response.output_audio.done"}"#);
        cap.observe("garbage");
        assert_eq!(
            cap.get_or(TextTurnSupport::Unknown),
            TextTurnSupport::Supported
        );
    }

    #[test]
    fn a_reconnect_does_not_inherit_the_previous_sessions_claim() {
        let cap = HandshakeCapability::new();
        cap.observe(r#"{"type":"session.created","session":{"modalities":["audio"]}}"#);
        cap.reset();
        assert_eq!(
            cap.get_or(TextTurnSupport::Unknown),
            TextTurnSupport::Unknown
        );
    }

    #[test]
    fn the_answer_is_shared_across_clones() {
        // The engine keeps one handle and hands another to its read task.
        let engine_side = HandshakeCapability::new();
        let reader_side = engine_side.clone();
        reader_side.observe(r#"{"type":"session.created","session":{"modalities":["text"]}}"#);
        assert_eq!(
            engine_side.get_or(TextTurnSupport::Unknown),
            TextTurnSupport::Supported
        );
    }

    #[test]
    fn session_created_parses_to_ready() {
        let raw = r#"{"type":"session.created","session":{"type":"realtime"}}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::Ready]
        ));
    }

    #[test]
    fn the_playback_bracket_opening_parses_to_speech_started() {
        let raw = r#"{"type":"output_audio_buffer.started","response_id":"resp_1"}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::SpeechStarted]
        ));
    }

    #[test]
    fn output_audio_done_parses_to_speech_ended() {
        let raw = r#"{"type":"response.output_audio.done","response_id":"resp_1"}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::SpeechEnded]
        ));
    }

    #[test]
    fn the_playback_bracket_closing_yields_no_events() {
        // `.stopped` trails `response.output_audio.done`; mapping both would emit
        // a duplicate SpeechEnded for one turn.
        let raw = r#"{"type":"output_audio_buffer.stopped","response_id":"resp_1"}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn recalled_lines_replay_as_conversation_items_under_their_own_speaker() {
        use uia_core::memory::{MemoryItem, MemoryKind, RECALL_ROLE_ASSISTANT, RECALL_ROLE_USER};
        let cfg = SessionConfig {
            memory_context: vec![
                MemoryItem {
                    content: "what is the capital of France".into(),
                    kind: MemoryKind::Other(RECALL_ROLE_USER.into()),
                    score: None,
                },
                MemoryItem {
                    content: "Paris".into(),
                    kind: MemoryKind::Other(RECALL_ROLE_ASSISTANT.into()),
                    score: None,
                },
            ],
            ..SessionConfig::default()
        };

        let events = memory_item_events(&cfg);

        assert_eq!(events.len(), 2, "one item per recalled line, in order");
        assert_eq!(events[0]["item"]["role"], "user");
        // Not interchangeable, and not symmetrical: `input_text` for the user,
        // `output_text` for the assistant. `text` is rejected by the API.
        assert_eq!(events[0]["item"]["content"][0]["type"], "input_text");
        assert_eq!(
            events[0]["item"]["content"][0]["text"],
            "what is the capital of France"
        );
        assert_eq!(events[1]["item"]["role"], "assistant");
        assert_eq!(events[1]["item"]["content"][0]["type"], "output_text");
        assert_eq!(events[1]["item"]["content"][0]["text"], "Paris");
        // Replaying history must never make the assistant start talking.
        assert!(
            events
                .iter()
                .all(|e| e["type"] == "conversation.item.create"),
            "history must not carry a response.create"
        );
    }

    #[test]
    fn a_recalled_line_with_no_usable_speaker_is_skipped_not_guessed() {
        use uia_core::memory::{MemoryItem, MemoryKind};
        let cfg = SessionConfig {
            memory_context: vec![
                MemoryItem {
                    content: "prefers metric units".into(),
                    kind: MemoryKind::Preference,
                    score: None,
                },
                MemoryItem {
                    content: "from some other backend".into(),
                    kind: MemoryKind::Other("turn".into()),
                    score: None,
                },
            ],
            ..SessionConfig::default()
        };

        assert!(
            memory_item_events(&cfg).is_empty(),
            "attributing a line to the wrong speaker teaches the model the              user said something they did not"
        );
    }

    #[test]
    fn no_memory_means_no_replay_events() {
        assert!(memory_item_events(&SessionConfig::default()).is_empty());
    }

    #[test]
    fn response_done_parses_to_turn_complete() {
        // The boundary the session records an exchange on. It must be its own
        // event: `response.output_audio.done` (SpeechEnded) can arrive before
        // `response.output_audio_transcript.done`, so recording at the audio
        // boundary splits one exchange across two entries.
        let raw = r#"{"type":"response.done","response":{"id":"resp_1"}}"#;
        assert!(matches!(
            parse_server_event(raw).unwrap().as_slice(),
            [EngineEvent::TurnComplete]
        ));
    }

    #[test]
    fn a_finished_transcript_parses_to_one_model_transcript() {
        let raw = r#"{"type":"response.output_audio_transcript.done",
                      "transcript":"It's 9:20 in Sydney right now."}"#;
        match &parse_server_event(raw).unwrap()[0] {
            EngineEvent::ModelTranscript(t) => {
                assert_eq!(t, "It's 9:20 in Sydney right now.")
            }
            other => panic!("expected ModelTranscript, got {other:?}"),
        }
    }

    #[test]
    fn a_completed_input_transcription_parses_to_one_user_transcript() {
        // S9 left this event name UNOBSERVED: it had no encoder, so it could
        // send no speech, so nothing was ever transcribed. Captured live in S16
        // by sending TTS speech with UIA_DUMP_EVENTS=1 -- this is the real
        // payload, verbatim, and the transcript is the sentence we sent.
        let raw = r#"{"type":"conversation.item.input_audio_transcription.completed",
                      "event_id":"event_EETUaZ33hQ75xs210nTPr",
                      "item_id":"item_EETUXSjP06ErgQk6NUJuo",
                      "content_index":0,
                      "transcript":"What is 2 plus 2?",
                      "usage":{"type":"duration","seconds":3}}"#;
        match &parse_server_event(raw).unwrap()[0] {
            EngineEvent::UserTranscript(t) => assert_eq!(t, "What is 2 plus 2?"),
            other => panic!("expected UserTranscript, got {other:?}"),
        }
    }

    #[test]
    fn input_transcription_deltas_yield_no_events() {
        // Symmetric with the model transcript: one event per turn, on
        // `.completed`. The server sends both a delta and a completed carrying
        // the same words, so emitting both would double-count every utterance.
        let raw = r#"{"type":"conversation.item.input_audio_transcription.delta",
                      "delta":"What is 2 plus 2?","obfuscation":"dNa0M9UjmlwpUCF"}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn transcript_deltas_yield_no_events() {
        // One transcript per turn, on `.done`. Nova emits whole utterances, and
        // S11 runs ONE contract suite against both engines.
        let raw = r#"{"type":"response.output_audio_transcript.delta","delta":"It's "}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn a_finished_function_call_parses_with_call_id_name_and_decoded_args() {
        let raw = r#"{"type":"response.function_call_arguments.done",
                      "arguments":"{\"city\":\"Sydney\"}",
                      "call_id":"call_YCHPiy0hRxEu1Nxk",
                      "item_id":"item_EDeSMI0F7AvxZiniAgCma","name":"get_time"}"#;
        match &parse_server_event(raw).unwrap()[0] {
            EngineEvent::ToolCall { id, name, args } => {
                // `call_id`, NOT `item_id`, is what the result must reference.
                assert_eq!(id, "call_YCHPiy0hRxEu1Nxk");
                assert_eq!(name, "get_time");
                // `arguments` arrives as a STRING holding JSON.
                assert_eq!(args["city"], "Sydney");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn function_call_argument_deltas_yield_no_events() {
        let raw = r#"{"type":"response.function_call_arguments.delta","delta":"{\"ci"}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn an_error_event_parses_to_a_transport_error_that_is_not_terminal() {
        let raw = r#"{"type":"error","error":{"type":"server_error",
                      "code":"rate_limit_exceeded","message":"slow down"}}"#;
        match &parse_server_event(raw).unwrap()[0] {
            EngineEvent::Error(e) => {
                assert!(e.to_string().contains("slow down"));
                assert!(!e.is_terminal(), "a rate limit must be retryable");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn an_authentication_error_event_is_terminal() {
        // Auth failures must never enter the backoff loop.
        let raw = r#"{"type":"error","error":{"type":"invalid_request_error",
                      "code":"invalid_api_key","message":"Incorrect API key provided"}}"#;
        match &parse_server_event(raw).unwrap()[0] {
            EngineEvent::Error(e) => assert!(e.is_terminal(), "got {e:?}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn a_cancellation_with_nothing_to_cancel_is_benign() {
        // Captured live 2026-08-19. Barge-in races the end of a response: the
        // user speaks exactly as it completes, `interrupt()` arrives late, and
        // OpenAI reports a failed cancellation. Surfacing that as an error would
        // classify a healthy session as transient-failed and reconnect it.
        let raw = r#"{"type":"error","event_id":"event_EER1Q4Ao1j1ULoqUKUivU",
                      "error":{"type":"invalid_request_error",
                      "code":"response_cancel_not_active",
                      "message":"Cancellation failed: no active response found",
                      "param":null,"event_id":null}}"#;
        assert!(
            parse_server_event(raw).unwrap().is_empty(),
            "a failed cancellation must not reach the session"
        );
    }

    #[test]
    fn an_unknown_event_yields_no_events_rather_than_an_error() {
        // Providers add events; an unrecognised one must not kill the session.
        let raw = r#"{"type":"conversation.item.added","item":{"id":"item_1"}}"#;
        assert!(parse_server_event(raw).unwrap().is_empty());
    }

    #[test]
    fn no_branch_reads_audio_bytes_off_the_data_channel() {
        // Structural: over WebRTC the audio is RTP media. The beta-era
        // `response.audio.delta` carrying base64 must produce NOTHING, or an
        // engine would appear to work while playing silence.
        let raw = r#"{"type":"response.audio.delta","delta":"AQACAA=="}"#;
        let evs = parse_server_event(raw).unwrap();
        assert!(
            !evs.iter().any(|e| matches!(e, EngineEvent::AudioChunk(_))),
            "audio must never come from the event channel: {evs:?}"
        );
        assert!(evs.is_empty());
    }

    #[test]
    fn malformed_json_is_a_protocol_error() {
        assert!(matches!(
            parse_server_event("not json at all"),
            Err(EngineError::Protocol(_))
        ));
    }

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

    #[test]
    fn a_namespaced_tool_name_is_sanitised_for_openai() {
        // OpenAI enforces ^[a-zA-Z0-9_-]+$ on tool names. S8's `namespaced()`
        // produces `server.tool` — the dot is REJECTED, and it takes the whole
        // session.update with it. Measured live 2026-08-19.
        assert_eq!(openai_tool_name("clock.now"), "clock__now");
        assert_eq!(openai_tool_name("plain_name"), "plain_name");
    }

    #[test]
    fn session_update_declares_the_sanitised_name() {
        let cfg = SessionConfig {
            tools: vec![a_tool()],
            ..SessionConfig::default()
        };
        assert_eq!(
            session_update(&cfg)["session"]["tools"][0]["name"],
            "clock__now"
        );
    }

    #[test]
    fn a_tool_call_resolves_back_to_the_namespaced_name() {
        // The executor is keyed by the ORIGINAL name; a round trip that loses
        // the namespace calls the wrong server, or nothing at all.
        let names = ToolNames::new(&[a_tool()]);
        assert_eq!(names.restore("clock__now"), "clock.now");
    }

    #[test]
    fn an_unknown_tool_name_passes_through_unchanged() {
        // Never invent a name for a tool we did not declare: let the executor
        // report "not found" rather than guessing an inverse.
        let names = ToolNames::new(&[a_tool()]);
        assert_eq!(names.restore("something_else"), "something_else");
    }

    #[test]
    fn sanitising_is_injective_across_the_declared_set() {
        // `.` -> `__` rather than `_` so `a.b` and `a_b` cannot collide.
        let a = ToolDescriptor {
            name: "a.b".into(),
            ..a_tool()
        };
        let b = ToolDescriptor {
            name: "a_b".into(),
            ..a_tool()
        };
        assert_ne!(openai_tool_name(&a.name), openai_tool_name(&b.name));
        let names = ToolNames::new(&[a, b]);
        assert_eq!(names.restore("a__b"), "a.b");
        assert_eq!(names.restore("a_b"), "a_b");
    }

    #[test]
    fn session_update_declares_each_tool_as_a_function_with_an_object_schema() {
        let cfg = SessionConfig {
            tools: vec![a_tool()],
            ..SessionConfig::default()
        };
        let update = session_update(&cfg);
        assert_eq!(update["type"], "session.update");
        assert_eq!(update["session"]["type"], "realtime");
        let tool = &update["session"]["tools"][0];
        assert_eq!(tool["type"], "function");
        // Sanitised: OpenAI refuses the dot in S8's `server.tool` names.
        assert_eq!(tool["name"], "clock__now");
        // An OBJECT, not Nova's stringified schema.
        assert_eq!(tool["parameters"]["properties"]["city"]["type"], "string");
        assert_eq!(update["session"]["tool_choice"], "auto");
    }

    #[test]
    fn text_turn_event_is_a_user_message_item_with_input_text_content() {
        let ev = text_turn_event("what's the weather?");
        assert_eq!(ev["type"], "conversation.item.create");
        assert_eq!(ev["item"]["type"], "message");
        assert_eq!(ev["item"]["role"], "user");
        assert_eq!(ev["item"]["content"][0]["type"], "input_text");
        assert_eq!(ev["item"]["content"][0]["text"], "what's the weather?");
    }

    #[test]
    fn the_system_prompt_rides_along_when_set() {
        let cfg = SessionConfig {
            system_prompt: Some("You are Assistant.".into()),
            ..SessionConfig::default()
        };
        assert_eq!(
            session_update(&cfg)["session"]["instructions"],
            "You are Assistant."
        );
    }

    #[test]
    fn transcription_is_absent_unless_configured() {
        let cfg = SessionConfig::default();
        assert!(!cfg.transcription);
        assert!(
            session_update(&cfg)["session"]["audio"]["input"]["transcription"].is_null(),
            "transcription costs latency and money; it is opt-in"
        );
    }

    #[test]
    fn transcription_is_requested_when_configured() {
        let cfg = SessionConfig {
            transcription: true,
            ..SessionConfig::default()
        };
        let update = session_update(&cfg);
        assert!(!update["session"]["audio"]["input"]["transcription"].is_null());
    }

    #[test]
    fn the_voice_is_set_when_configured() {
        let cfg = SessionConfig {
            voice: Some("shimmer".to_string()),
            ..SessionConfig::default()
        };
        let update = session_update(&cfg);
        assert_eq!(
            update["session"]["audio"]["output"]["voice"], "shimmer",
            "the selected voice must be passed in the session update, nested \
             under audio.output per the GA gpt-realtime schema"
        );
    }

    #[test]
    fn voice_and_transcription_coexist_under_the_same_audio_object() {
        let cfg = SessionConfig {
            voice: Some("marin".to_string()),
            transcription: true,
            ..SessionConfig::default()
        };
        let update = session_update(&cfg);
        assert_eq!(update["session"]["audio"]["output"]["voice"], "marin");
        assert!(!update["session"]["audio"]["input"]["transcription"].is_null());
    }
}
