// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! `S2sEngine` for Nova Sonic over `InvokeModelWithBidirectionalStream`.
//!
//! Call sequence is the one S1 proved end to end (findings §2.1, §2.3):
//! `sessionStart` → `promptStart` (carrying `to_nova_declaration` per tool) →
//! optional system `contentStart`/`textInput`/`contentEnd` → `contentStart`
//! (AUDIO, `interactive: true`, opened once and kept open for the session) →
//! `audioInput`×N as `send_audio` is called. `validate_region` runs before any
//! AWS call.
//!
//! **The session-setup events must already be queued before `send()` is
//! awaited** (findings §2.1) — Nova emits no response headers until it has
//! received `sessionStart`, and queueing afterwards deadlocks silently.

use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::primitives::Blob;
use aws_sdk_bedrockruntime::types::{
    BidirectionalInputPayloadPart, InvokeModelWithBidirectionalStreamInput,
    InvokeModelWithBidirectionalStreamOutput, error::InvokeModelWithBidirectionalStreamInputError,
};
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use uia_core::audio::AudioFormat;
use uia_core::engine::{
    EngineError, EngineEvent, EngineId, S2sEngine, SessionConfig, TextTurnSupport, ToolCallId,
};
use uia_core::tools::ToolResult;

use crate::protocol;

/// Verified against the Bedrock control plane 2026-08-16. Never guess or
/// construct a Bedrock model id.
/// Bedrock has exactly one Nova Sonic model today. Exposed as a
/// *default* rather than a fixed constant so `[bedrock] model` has the
/// same shape as `[openai] model` and `[foundry] deployment`: the day
/// AWS ships a second Sonic id, pinning it is a config edit, not a
/// code change.
pub const DEFAULT_MODEL_ID: &str = "amazon.nova-2-sonic-v1:0";

/// Lowest RTT from Australia: ~110-130ms, against ~140-170ms to Oregon and
/// 200ms+ to Virginia (findings §1).
pub const DEFAULT_REGION: &str = "ap-northeast-1";

/// The only regions hosting the model. There is no global endpoint and no
/// inference profile: `inferenceTypesSupported` is `ON_DEMAND` on the base id.
pub const SUPPORTED_REGIONS: [&str; 3] = ["us-east-1", "us-west-2", "ap-northeast-1"];

/// Nova's ASR sweet spot, and the only rate all of Polly, cpal defaults, and
/// Nova agree on cheaply (findings §1). Nova rejects anything outside
/// `{8000, 16000, 24000}`.
const INPUT_SAMPLE_RATE_HZ: u32 = 16_000;

/// Whatever we request in `promptStart.audioOutputConfiguration` — the
/// service echoes it back exactly (findings §2.2). 24 kHz is what S1 verified.
const OUTPUT_SAMPLE_RATE_HZ: u32 = 24_000;

const PROMPT_NAME: &str = "uia-prompt";
const SYSTEM_CONTENT: &str = "system-prompt";
const AUDIO_CONTENT: &str = "user-audio";

/// Prefix for a replayed history block. One block per recalled line, each
/// needing its own name for the same reason a typed turn does: `contentName`
/// identifies an open block, so reusing one name would have each line close
/// the block the last one opened.
const HISTORY_CONTENT_PREFIX: &str = "history-";

/// Prefix for a typed turn's content block. Unlike `SYSTEM_CONTENT` and
/// `AUDIO_CONTENT` — one block each per session — every typed turn opens and
/// closes its own block, so the name carries a per-session counter
/// (`next_text_content_name`). Same shape as the per-tool `tool-{id}` naming
/// below, for the same reason: Nova keys content by `contentName`, and reusing
/// one across two blocks would conflate them.
const TEXT_CONTENT_PREFIX: &str = "user-text";

type InputEvent =
    Result<InvokeModelWithBidirectionalStreamInput, InvokeModelWithBidirectionalStreamInputError>;

pub struct NovaEngine {
    region: String,
    access_key_id: String,
    secret_access_key: String,
    model_id: String,
    conn: Option<Connection>,
    /// Monotonic counter behind `next_text_content_name`. Deliberately not
    /// reset by `connect`: a name only has to be unique, and never reusing one
    /// across a reconnect is strictly safer than restarting at zero.
    text_turns_sent: u64,
}

struct Connection {
    input_tx: mpsc::Sender<InputEvent>,
    reader: tokio::task::JoinHandle<()>,
}

impl NovaEngine {
    pub const DEFAULT_MODEL_ID: &'static str = DEFAULT_MODEL_ID;
    pub const DEFAULT_REGION: &'static str = DEFAULT_REGION;
    pub const SUPPORTED_REGIONS: [&'static str; 3] = SUPPORTED_REGIONS;

    /// Takes IAM access keys explicitly rather than reading the ambient AWS
    /// credential chain. `InvokeModelWithBidirectionalStream` cannot be
    /// authenticated with a Bedrock API key (bearer token) — AWS docs list it
    /// as one of the operations Bedrock API keys explicitly do not cover — so
    /// SigV4 access keys are the only credential shape that works here. Taking
    /// them as explicit arguments (instead of `aws_config::defaults()`) means
    /// this also never silently succeeds only because the caller's machine
    /// happens to have an SSO session active.
    pub fn new(
        region: String,
        access_key_id: String,
        secret_access_key: String,
        model_id: String,
    ) -> Self {
        Self {
            region,
            access_key_id,
            secret_access_key,
            model_id,
            conn: None,
            text_turns_sent: 0,
        }
    }

    /// The Bedrock model id `connect` will open the bidirectional stream
    /// against.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// A fresh, distinct `contentName` for each typed turn (PLAN.md FD8).
    fn next_text_content_name(&mut self) -> String {
        self.text_turns_sent += 1;
        format!("{TEXT_CONTENT_PREFIX}-{}", self.text_turns_sent)
    }

    pub fn validate_region(region: &str) -> Result<(), EngineError> {
        if Self::SUPPORTED_REGIONS.contains(&region) {
            return Ok(());
        }
        Err(EngineError::Protocol(format!(
            "Nova Sonic is not available in {region}. Supported: {}. Default: {}.",
            Self::SUPPORTED_REGIONS.join(", "),
            Self::DEFAULT_REGION
        )))
    }
}

/// Queue one event onto the input stream. A send failure means the reader
/// side (and therefore the stream) is gone.
async fn queue(tx: &mpsc::Sender<InputEvent>, event: serde_json::Value) -> Result<(), EngineError> {
    let part = BidirectionalInputPayloadPart::builder()
        .bytes(Blob::new(event.to_string().into_bytes()))
        .build();
    tx.send(Ok(InvokeModelWithBidirectionalStreamInput::Chunk(part)))
        .await
        .map_err(|_| EngineError::Closed)
}

/// Flatten an SDK error into something a human can act on.
///
/// The AWS SDK's outer `Display` is famously uninformative — an
/// `InvokeModelWithBidirectionalStream` rejection renders as the bare string
/// "service error", with the ValidationException and its message hidden one or
/// more levels down the `source()` chain. S14 lost a full A/B run to that: ten
/// identical "transport error: service error" lines that named neither the
/// field Bedrock objected to nor the fact that it was a validation failure at
/// all. Walking the chain is also what makes `classify_sdk_error`'s keyword
/// match work, since the auth keywords live in the source too, not the summary.
fn describe(err: &dyn std::error::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(e) = source {
        let text = e.to_string();
        if !text.is_empty() && !parts.contains(&text) {
            parts.push(text);
        }
        source = e.source();
    }
    parts.join(": ")
}

/// Auth failures must never enter the backoff loop; everything else is
/// transient, so a wrong answer here means reconnecting forever against a
/// credential that will never work.
///
/// Verified live 2026-08-22 (SP2 Task 3, see
/// `an_invalid_credential_pair_is_classified_as_auth_not_transport` in
/// tests/contract.rs): an invalid key pair makes Bedrock return
/// `UnrecognizedClientException` / "The security token included in the request
/// is invalid.", which `"unrecognizedclient"` below already matches. The list
/// was inferred from the SDK's exception type names and is no longer
/// unexercised guesswork — at least for this, the common case.
///
/// Only reachable because [`describe`] walks the whole `source()` chain: the
/// exception name lives several levels down, and the outer `Display` is the
/// bare string "service error".
fn classify_sdk_error(err: impl std::error::Error) -> EngineError {
    let msg = describe(&err);
    let lower = msg.to_lowercase();
    let is_auth = [
        "accessdenied",
        "unrecognizedclient",
        "unauthorized",
        "expiredtoken",
        "forbidden",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if is_auth {
        EngineError::Auth(msg)
    } else {
        EngineError::Transport(msg)
    }
}

#[async_trait::async_trait]
impl S2sEngine for NovaEngine {
    fn id(&self) -> EngineId {
        EngineId::NovaSonic
    }

    fn input_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(INPUT_SAMPLE_RATE_HZ)
    }

    fn output_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(OUTPUT_SAMPLE_RATE_HZ)
    }

    async fn connect(
        &mut self,
        cfg: &SessionConfig,
        tx: Sender<EngineEvent>,
    ) -> Result<(), EngineError> {
        // Fail loudly here rather than a confusing ValidationException later.
        Self::validate_region(&self.region)?;

        // Explicit static credentials, not `aws_config::defaults()`: the
        // default chain would pick up an SSO profile or `~/.aws/credentials`
        // on a developer machine, masking the fact that an end user's machine
        // has neither and would fail here with no clear reason why.
        let credentials = aws_sdk_bedrockruntime::config::Credentials::new(
            &self.access_key_id,
            &self.secret_access_key,
            None,
            None,
            "uia-nova-static",
        );
        let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(self.region.clone()))
            .credentials_provider(credentials)
            .load()
            .await;
        let client = Client::new(&aws_cfg);

        let (input_tx, input_rx) = mpsc::channel::<InputEvent>(64);

        // GOTCHA (findings §2.1): every setup event must already be in the
        // channel before `send()` is awaited, or the stream hangs forever
        // with no error and no output.
        queue(&input_tx, protocol::session_start_event()).await?;
        queue(
            &input_tx,
            protocol::prompt_start_event(PROMPT_NAME, &cfg.tools, OUTPUT_SAMPLE_RATE_HZ),
        )
        .await?;
        if let Some(prompt) = &cfg.system_prompt {
            queue(
                &input_tx,
                protocol::content_start_text_event(PROMPT_NAME, SYSTEM_CONTENT, "SYSTEM", false),
            )
            .await?;
            queue(
                &input_tx,
                protocol::text_input_event(PROMPT_NAME, SYSTEM_CONTENT, prompt),
            )
            .await?;
            queue(
                &input_tx,
                protocol::content_end_event(PROMPT_NAME, SYSTEM_CONTENT),
            )
            .await?;
        }
        // What was said last time, oldest first, before the live audio block
        // opens. `interactive: false` is what keeps these history rather than
        // conversation: an interactive block is a turn Nova generates off once
        // it closes, which would have the assistant answer the last thing said
        // in the previous session the moment this one connects.
        //
        // An unrecognised role tag is skipped rather than guessed at —
        // replaying a line under the wrong speaker teaches the model the user
        // said something they did not.
        for (i, item) in cfg.memory_context.iter().enumerate() {
            let uia_core::memory::MemoryKind::Other(role) = &item.kind else {
                continue;
            };
            let role = match role.as_str() {
                uia_core::memory::RECALL_ROLE_USER => "USER",
                uia_core::memory::RECALL_ROLE_ASSISTANT => "ASSISTANT",
                _ => continue,
            };
            let content_name = format!("{HISTORY_CONTENT_PREFIX}{i}");
            queue(
                &input_tx,
                protocol::content_start_text_event(PROMPT_NAME, &content_name, role, false),
            )
            .await?;
            queue(
                &input_tx,
                protocol::text_input_event(PROMPT_NAME, &content_name, &item.content),
            )
            .await?;
            queue(
                &input_tx,
                protocol::content_end_event(PROMPT_NAME, &content_name),
            )
            .await?;
        }
        // Opened once, kept open for the whole session (`interactive: true`):
        // turns are the model's own VAD, not a per-turn re-open.
        queue(
            &input_tx,
            protocol::content_start_audio_event(PROMPT_NAME, AUDIO_CONTENT, INPUT_SAMPLE_RATE_HZ),
        )
        .await?;

        let body = ReceiverStream::new(input_rx).into();
        let mut out = client
            .invoke_model_with_bidirectional_stream()
            .model_id(self.model_id.clone())
            .body(body)
            .send()
            .await
            .map_err(classify_sdk_error)?;

        // `send()` returning is Nova's only "ready" signal — there is no
        // explicit session-ready event on the wire (contrast OpenAI's
        // `session.created`).
        let _ = tx.send(EngineEvent::Ready).await;

        // Built from the tools THIS session declared, and moved into the
        // reader: it is the only place that knows the mapping, and a name
        // rewritten by string alone would be ambiguous.
        let tool_names = uia_mcp::ToolNames::new(&cfg.tools);

        let reader = tokio::spawn(async move {
            loop {
                match out.body.recv().await {
                    Ok(Some(InvokeModelWithBidirectionalStreamOutput::Chunk(part))) => {
                        let Some(bytes) = part.bytes() else {
                            continue;
                        };
                        let text = String::from_utf8_lossy(bytes.as_ref());
                        if std::env::var("UIA_DUMP_EVENTS").is_ok() {
                            eprintln!("RAW {text}");
                        }
                        match protocol::parse_server_event(&text) {
                            Ok(events) => {
                                for e in protocol::restore_tool_names(events, &tool_names) {
                                    if tx.send(e).await.is_err() {
                                        return; // receiver gone
                                    }
                                }
                            }
                            // A malformed event is reported, never fatal.
                            Err(e) => {
                                if tx.send(EngineEvent::Error(e)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        let _ = tx.send(EngineEvent::Closed).await;
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(EngineEvent::Error(classify_sdk_error(e))).await;
                        return;
                    }
                }
            }
        });

        self.conn = Some(Connection { input_tx, reader });
        Ok(())
    }

    async fn send_audio(&mut self, frame: &[i16]) -> Result<(), EngineError> {
        let conn = self.conn.as_ref().ok_or(EngineError::Closed)?;
        let b64 = protocol::encode_pcm16(frame);
        queue(
            &conn.input_tx,
            protocol::audio_input_event(PROMPT_NAME, AUDIO_CONTENT, &b64),
        )
        .await
    }

    /// Nova 2 Sonic's cross-modal text turn: one TEXT content block, opened
    /// `interactive: true` as `USER`, filled, and closed (PLAN.md FD8). The
    /// audio block opened at connect stays open alongside it — the two are
    /// distinct `contentName`s on the same `promptName`.
    ///
    /// There is deliberately no fourth event: Nova has no `response.create`
    /// analogue and generates off the closed block on its own (FD9).
    async fn send_text(&mut self, text: &str) -> Result<(), EngineError> {
        // Take the name before borrowing `conn` — both need `self`.
        let content_name = self.next_text_content_name();
        let conn = self.conn.as_ref().ok_or(EngineError::Closed)?;
        queue(
            &conn.input_tx,
            protocol::content_start_text_event(PROMPT_NAME, &content_name, "USER", true),
        )
        .await?;
        queue(
            &conn.input_tx,
            protocol::text_input_event(PROMPT_NAME, &content_name, text),
        )
        .await?;
        queue(
            &conn.input_tx,
            protocol::content_end_event(PROMPT_NAME, &content_name),
        )
        .await
    }

    async fn send_tool_result(
        &mut self,
        id: ToolCallId,
        result: ToolResult,
    ) -> Result<(), EngineError> {
        let conn = self.conn.as_ref().ok_or(EngineError::Closed)?;
        let content_name = format!("tool-{id}");
        queue(
            &conn.input_tx,
            protocol::content_start_tool_event(PROMPT_NAME, &content_name, &id),
        )
        .await?;
        queue(
            &conn.input_tx,
            protocol::tool_result_event(PROMPT_NAME, &content_name, &result.content),
        )
        .await?;
        queue(
            &conn.input_tx,
            protocol::content_end_event(PROMPT_NAME, &content_name),
        )
        .await
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
        // Unlike OpenAI's `response.cancel`, Nova's bidirectional stream has
        // no explicit client-initiated cancel. Verified live (SP2 Task 4,
        // see interrupt_during_a_live_response_does_not_error_and_stream_keeps_working):
        // calling interrupt() mid-response sends nothing over the wire and
        // never errors, but it also does NOT stop server-side generation —
        // in the spike, Bedrock kept streaming audio for the in-flight
        // utterance and then generated two further unprompted follow-up
        // utterances afterward, all unaffected by the call. The local sink
        // clear (PLAN.md) is authoritative for what the user hears, so this
        // silences playback correctly, but every interrupted turn still
        // burns full Bedrock generation cost server-side. That's a known
        // limitation, not a bug in this no-op — a real fix needs a Bedrock
        // client-cancel event, which doesn't currently exist for this API.
        if self.conn.is_none() {
            return Err(EngineError::Closed);
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<(), EngineError> {
        if let Some(conn) = self.conn.take() {
            let _ = queue(
                &conn.input_tx,
                protocol::content_end_event(PROMPT_NAME, AUDIO_CONTENT),
            )
            .await;
            let _ = queue(&conn.input_tx, protocol::prompt_end_event(PROMPT_NAME)).await;
            let _ = queue(&conn.input_tx, protocol::session_end_event()).await;
            drop(conn.input_tx);
            conn.reader.abort();
        }
        Ok(())
    }

    /// `Supported` as of S1: `send_text` above implements Nova 2 Sonic's
    /// cross-modal text turn, so the claim is now true. This is statically
    /// `Supported` rather than handshake-derived (PLAN.md FD4) because
    /// Bedrock's Sonic catalog has exactly one entry — there is no
    /// per-deployment variation to detect. The model id is configurable
    /// (`[bedrock] model`) rather than hardcoded as FD7 assumed, so this is
    /// now a claim about the catalog, not about the code: a hand-pinned id
    /// that does not support text turns falls to FD3's backend degrade,
    /// which takes the session to `Unsupported` with the real error as the
    /// reason.
    fn text_turn_support(&self) -> TextTurnSupport {
        TextTurnSupport::Supported
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_model_id_is_the_verified_constant() {
        assert_eq!(NovaEngine::DEFAULT_MODEL_ID, "amazon.nova-2-sonic-v1:0");
    }

    #[test]
    fn the_engine_streams_against_the_model_id_it_was_built_with() {
        // Bedrock has exactly one Sonic model today, so the interesting case
        // is the second one: a pinned id must reach the stream unchanged
        // rather than the default being re-read underneath it. Same rule as
        // `OpenAiEngine`'s `model` and `FoundryEngine`'s deployment.
        let default = NovaEngine::new(
            NovaEngine::DEFAULT_REGION.into(),
            "ak".into(),
            "sk".into(),
            NovaEngine::DEFAULT_MODEL_ID.into(),
        );
        assert_eq!(default.model_id(), "amazon.nova-2-sonic-v1:0");

        let pinned = NovaEngine::new(
            NovaEngine::DEFAULT_REGION.into(),
            "ak".into(),
            "sk".into(),
            "amazon.nova-9-sonic-v1:0".into(),
        );
        assert_eq!(
            pinned.model_id(),
            "amazon.nova-9-sonic-v1:0",
            "a pinned model id must not fall back to the default"
        );
    }

    #[test]
    fn region_defaults_to_tokyo_and_rejects_the_home_region() {
        assert_eq!(NovaEngine::DEFAULT_REGION, "ap-northeast-1");
        assert!(NovaEngine::validate_region("ap-northeast-1").is_ok());
        assert!(NovaEngine::validate_region("us-west-2").is_ok());
        assert!(NovaEngine::validate_region("us-east-1").is_ok());
        let err = NovaEngine::validate_region("ap-southeast-2");
        assert!(
            err.is_err(),
            "Sydney has no Nova Sonic and must be rejected"
        );
    }

    #[test]
    fn unsupported_regions_are_rejected_with_a_helpful_message() {
        let err = NovaEngine::validate_region("eu-west-1").unwrap_err();
        assert!(err.to_string().contains("ap-northeast-1"), "got: {err}");
    }

    #[test]
    fn formats_match_what_s1_measured_live() {
        let e = NovaEngine::new(
            DEFAULT_REGION.to_string(),
            "test-key".into(),
            "test-secret".into(),
            DEFAULT_MODEL_ID.into(),
        );
        assert_eq!(e.input_format(), AudioFormat::mono_pcm16(16_000));
        assert_eq!(e.output_format(), AudioFormat::mono_pcm16(24_000));
        assert_eq!(e.id(), EngineId::NovaSonic);
    }

    #[tokio::test]
    async fn sending_before_connecting_is_closed_not_a_panic() {
        let mut e = NovaEngine::new(
            DEFAULT_REGION.to_string(),
            "test-key".into(),
            "test-secret".into(),
            DEFAULT_MODEL_ID.into(),
        );
        assert!(matches!(e.interrupt().await, Err(EngineError::Closed)));
        assert!(matches!(
            e.send_audio(&[1, 2, 3]).await,
            Err(EngineError::Closed)
        ));
        assert!(matches!(e.send_text("hi").await, Err(EngineError::Closed)));
        assert!(matches!(
            e.send_tool_result("call_1".into(), ToolResult::ok("{}"))
                .await,
            Err(EngineError::Closed)
        ));
        // close() on a never-connected engine is a no-op, not an error.
        assert!(e.close().await.is_ok());
    }

    #[test]
    fn text_turns_are_reported_supported() {
        // The trait method is the source of truth for this fact (PLAN.md FD2),
        // and as of S1 the claim is backed by a real `send_text`. It carries no
        // reason precisely because there is nothing to explain away.
        let e = NovaEngine::new(
            DEFAULT_REGION.to_string(),
            "test-key".into(),
            "test-secret".into(),
            DEFAULT_MODEL_ID.into(),
        );
        let support = e.text_turn_support();
        assert!(
            matches!(support, TextTurnSupport::Supported),
            "got: {support:?}"
        );
        assert!(support.is_usable());
        assert_eq!(support.reason(), None);
    }

    /// An engine wired to a channel we can read, standing in for a live
    /// bidirectional stream. The reader half is returned so a test can assert
    /// on the exact bytes `send_text` puts on the wire.
    fn engine_on_a_fake_stream() -> (NovaEngine, mpsc::Receiver<InputEvent>) {
        let (input_tx, input_rx) = mpsc::channel::<InputEvent>(64);
        let mut e = NovaEngine::new(
            DEFAULT_REGION.to_string(),
            "test-key".into(),
            "test-secret".into(),
            DEFAULT_MODEL_ID.into(),
        );
        e.conn = Some(Connection {
            input_tx,
            reader: tokio::spawn(async {}),
        });
        (e, input_rx)
    }

    /// Drain everything queued so far, decoded back to JSON.
    fn drain(rx: &mut mpsc::Receiver<InputEvent>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            let InvokeModelWithBidirectionalStreamInput::Chunk(part) = item.unwrap() else {
                panic!("only chunks are ever queued");
            };
            let bytes = part.bytes().expect("chunk carries bytes").as_ref().to_vec();
            out.push(serde_json::from_slice(&bytes).expect("valid JSON on the wire"));
        }
        out
    }

    #[tokio::test]
    async fn a_typed_turn_is_content_start_text_input_content_end() {
        let (mut e, mut rx) = engine_on_a_fake_stream();
        e.send_text("what's the weather?").await.unwrap();

        let events = drain(&mut rx);
        assert_eq!(
            events.len(),
            3,
            "exactly three events, no more (FD9): {events:?}"
        );

        let start = &events[0]["event"]["contentStart"];
        let content_name = start["contentName"].as_str().expect("a contentName");
        assert_eq!(start["promptName"], PROMPT_NAME);
        assert_eq!(start["type"], "TEXT");
        assert_eq!(start["role"], "USER");
        assert_eq!(
            start["interactive"], true,
            "a typed turn is a live turn, not setup (FD8)"
        );
        assert_eq!(start["textInputConfiguration"]["mediaType"], "text/plain");

        let input = &events[1]["event"]["textInput"];
        assert_eq!(input["promptName"], PROMPT_NAME);
        assert_eq!(input["contentName"], content_name);
        assert_eq!(input["content"], "what's the weather?");

        let end = &events[2]["event"]["contentEnd"];
        assert_eq!(end["promptName"], PROMPT_NAME);
        assert_eq!(end["contentName"], content_name);
    }

    #[tokio::test]
    async fn a_typed_turn_uses_its_own_content_name_not_the_audio_block_s() {
        // The audio block opened at connect stays open alongside the text one;
        // sharing a contentName would conflate two live blocks.
        let (mut e, mut rx) = engine_on_a_fake_stream();
        e.send_text("hello").await.unwrap();
        let name = drain(&mut rx)[0]["event"]["contentStart"]["contentName"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(name, AUDIO_CONTENT);
        assert_ne!(name, SYSTEM_CONTENT);
    }

    #[tokio::test]
    async fn two_typed_turns_get_two_different_content_names() {
        let (mut e, mut rx) = engine_on_a_fake_stream();
        e.send_text("first").await.unwrap();
        e.send_text("second").await.unwrap();

        let events = drain(&mut rx);
        assert_eq!(events.len(), 6, "three events per turn");
        let first = &events[0]["event"]["contentStart"]["contentName"];
        let second = &events[3]["event"]["contentStart"]["contentName"];
        assert_ne!(
            first, second,
            "each turn opens its own content block (FD8): {events:?}"
        );
        // ...and each turn's own three events still agree with each other.
        assert_eq!(&events[4]["event"]["textInput"]["contentName"], second);
        assert_eq!(&events[5]["event"]["contentEnd"]["contentName"], second);
    }

    #[tokio::test]
    async fn connect_rejects_the_home_region_before_any_aws_call() {
        // No credentials or network are configured in this test process; a
        // rejection that reaches the AWS call would hang or error differently
        // (credential resolution), not with this specific message.
        let mut e = NovaEngine::new(
            "ap-southeast-2".to_string(),
            "test-key".into(),
            "test-secret".into(),
            DEFAULT_MODEL_ID.into(),
        );
        let (tx, _rx) = mpsc::channel(1);
        let err = e.connect(&SessionConfig::default(), tx).await.unwrap_err();
        assert!(err.to_string().contains("ap-northeast-1"), "got: {err}");
    }
}
