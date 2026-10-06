// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

pub mod fake;
pub use fake::{AudioProbe, ConfigProbe, FakeEngine, InterruptProbe, ToolResultProbe};

use crate::audio::AudioFormat;
use crate::memory::MemoryItem;
use crate::tools::{ToolDescriptor, ToolResult};
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;

pub type ToolCallId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineId {
    OpenAi,
    NovaSonic,
    Foundry,
}

#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolDescriptor>,
    /// Prior context injected at setup. Empty when memory is disabled or
    /// when recall exceeded its deadline.
    pub memory_context: Vec<MemoryItem>,
    /// Off by default: transcription costs latency and money, and nothing
    /// consumes it until a memory backend is enabled.
    pub transcription: bool,
    pub voice: Option<String>,
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Ready,
    SpeechStarted,
    AudioChunk(Vec<i16>),
    SpeechEnded,
    /// The exchange is over and can be written down — every transcript that
    /// belongs to it has now been sent.
    ///
    /// Deliberately NOT `SpeechEnded`. That marks the assistant's *playback*
    /// bracket closing, and the transcripts do not respect it: OpenAI's
    /// `response.output_audio.done` can arrive before
    /// `response.output_audio_transcript.done`, so a turn recorded at
    /// `SpeechEnded` captured whichever halves happened to have landed and
    /// pushed the rest into the following turn. That off-by-one is what made
    /// the stored conversation a stream of one-sided entries.
    ///
    /// An engine that has no separate boundary emits this alongside
    /// `SpeechEnded` from the same event, which is exactly what its ordering
    /// guarantees already amount to.
    TurnComplete,
    UserTranscript(String),
    ModelTranscript(String),
    ToolCall {
        id: ToolCallId,
        name: String,
        args: serde_json::Value,
    },
    Interrupted,
    Error(EngineError),
    Closed,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum EngineError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("engine closed")]
    Closed,
}

impl EngineError {
    /// Terminal errors must NOT trigger backoff reconnection. An auth failure
    /// retried behind a spinner is the single most-misdiagnosed failure mode.
    pub fn is_terminal(&self) -> bool {
        matches!(self, EngineError::Auth(_))
    }
}

/// Whether an engine can accept a user turn typed as text on a live
/// speech-to-speech session (PLAN.md FD2). Three states, not a `bool`,
/// because "we could not tell" is a genuinely different answer from "no" and
/// the two must not collapse: by FD3 an `Unknown` engine keeps the user's
/// input and degrades only on a real failure, while an `Unsupported` one
/// disables it up front with a reason the UI can show.
///
/// Serde-serializable because it crosses the Tauri IPC boundary — the shell
/// reads it through one command and one event (`crates/uia-app/src/hud.rs`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TextTurnSupport {
    /// Typed turns work on this engine, today, as configured.
    Supported,
    /// Typed turns do not work, and this is why — the string is shown to the
    /// user verbatim next to the disabled control (FD6), so it must read as
    /// an explanation rather than an error code.
    Unsupported(String),
    /// Nothing authoritative said either way. Per FD3 this means "assume
    /// capable": the control stays live and a real failure is what demotes
    /// it, never a guess.
    Unknown,
}

impl TextTurnSupport {
    /// Whether a UI should leave the typed-turn control enabled. `Unknown`
    /// counts as usable — that is the whole point of FD3.
    pub fn is_usable(&self) -> bool {
        !matches!(self, TextTurnSupport::Unsupported(_))
    }

    /// The reason a control is disabled, if there is one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            TextTurnSupport::Unsupported(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

#[async_trait]
pub trait S2sEngine: Send {
    fn id(&self) -> EngineId;
    fn input_format(&self) -> AudioFormat;
    fn output_format(&self) -> AudioFormat;
    async fn connect(
        &mut self,
        cfg: &SessionConfig,
        tx: Sender<EngineEvent>,
    ) -> Result<(), EngineError>;
    async fn send_audio(&mut self, frame: &[i16]) -> Result<(), EngineError>;
    async fn send_tool_result(
        &mut self,
        id: ToolCallId,
        result: ToolResult,
    ) -> Result<(), EngineError>;
    async fn interrupt(&mut self) -> Result<(), EngineError>;
    async fn close(&mut self) -> Result<(), EngineError>;

    /// Submit a user turn as text rather than audio (PLAN.md S4). Most
    /// engines have no text-input path onto a speech-to-speech session, so
    /// the default is a clear, named failure rather than a silent no-op —
    /// callers (the typed-turn Tauri command) surface this string directly,
    /// so it must read as an answer, not a stack trace.
    async fn send_text(&mut self, _text: &str) -> Result<(), EngineError> {
        Err(EngineError::Protocol(
            "text turns are not supported by this engine".into(),
        ))
    }

    /// This engine's honest answer to "can this session take a typed turn?"
    /// (PLAN.md FD2) — the single signal every surface reads, replacing the
    /// per-engine ad-hoc checks the shell used to make.
    ///
    /// The default is [`TextTurnSupport::Unknown`], not `Unsupported`, even
    /// though the `send_text` default above errors: an engine that has not
    /// overridden this has made no claim either way, and FD3 says an unmade
    /// claim must never cost the user their input. An engine that genuinely
    /// cannot take typed turns says so explicitly.
    fn text_turn_support(&self) -> TextTurnSupport {
        TextTurnSupport::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn fake_engine_emits_its_scripted_events_in_order() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut engine = FakeEngine::new(vec![
            EngineEvent::Ready,
            EngineEvent::SpeechStarted,
            EngineEvent::AudioChunk(vec![7, 7]),
            EngineEvent::SpeechEnded,
        ]);
        engine.connect(&SessionConfig::default(), tx).await.unwrap();

        assert!(matches!(rx.recv().await, Some(EngineEvent::Ready)));
        assert!(matches!(rx.recv().await, Some(EngineEvent::SpeechStarted)));
        assert!(matches!(rx.recv().await, Some(EngineEvent::AudioChunk(c)) if c == vec![7, 7]));
        assert!(matches!(rx.recv().await, Some(EngineEvent::SpeechEnded)));
    }

    #[tokio::test]
    async fn fake_engine_records_audio_sent_to_it() {
        let (tx, _rx) = mpsc::channel(16);
        let mut engine = FakeEngine::new(vec![EngineEvent::Ready]);
        engine.connect(&SessionConfig::default(), tx).await.unwrap();
        engine.send_audio(&[1, 2, 3]).await.unwrap();
        assert_eq!(engine.sent_audio(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn default_send_text_is_a_clear_not_supported_error() {
        // Every real engine overrides this; the default is for a new engine
        // that has not implemented a text path yet.
        let mut engine = FakeEngine::new(vec![]);
        let err = engine.send_text("hi").await.unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("not supported"),
            "got: {err}"
        );
    }

    #[test]
    fn auth_errors_are_terminal_and_others_are_not() {
        // Drives the "never retry-loop on auth" rule in the error table.
        assert!(EngineError::Auth("bad key".into()).is_terminal());
        assert!(!EngineError::Transport("reset".into()).is_terminal());
        assert!(!EngineError::Protocol("bad frame".into()).is_terminal());
    }
}
