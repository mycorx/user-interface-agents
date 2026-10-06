// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The real run loop — what `uia-app` drives, as opposed to the
//! deterministic `run_until_*` drivers the earlier session tests use.
//!
//! S14 exists because nothing before it owned "compose the whole thing and
//! actually run it": `connect()` was private, state transitions were
//! unobservable from outside the crate, and `Backoff` (S6) existed but nothing
//! ever called it. These tests pin all three.

use tokio::sync::mpsc::Sender;
use uia_core::activation::{ActivationEvent, ChannelActivation};
use uia_core::audio::{AudioFormat, FixtureSource, VecSink};
use uia_core::engine::{
    EngineError, EngineEvent, EngineId, FakeEngine, S2sEngine, SessionConfig, ToolCallId,
};
use uia_core::memory::{MemoryContext, NullMemory};
use uia_core::session::{RunOutcome, Session, SessionDeps, State};
use uia_core::tools::{FakeExecutor, ToolResult};

fn ctx() -> MemoryContext {
    MemoryContext {
        user_id: "test-user".into(),
        conversation_id: "test-convo".into(),
    }
}

fn deps(
    engine: Box<dyn S2sEngine>,
    frames: Vec<Vec<i16>>,
) -> (SessionDeps, tokio::sync::mpsc::Sender<ActivationEvent>) {
    let (activation, act_tx) = ChannelActivation::new();
    (
        SessionDeps {
            source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), frames)),
            sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
            engine,
            executor: std::sync::Arc::new(FakeExecutor::new()),
            memory: std::sync::Arc::new(NullMemory),
            activation: Box::new(activation),
            ctx: ctx(),
        },
        act_tx,
    )
}

#[tokio::test]
async fn every_state_transition_is_broadcast_so_the_hud_never_misses_one() {
    // A `watch` would coalesce: Connecting -> Listening happens in microseconds
    // and the HUD would only ever render Listening, so "status text tracks Idle
    // -> Connecting -> Listening" (MANUAL-TEST.md) could not be observed at all.
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::SpeechStarted,
        EngineEvent::AudioChunk(vec![5, 6, 7]),
        EngineEvent::SpeechEnded,
    ]);
    let (d, act_tx) = deps(Box::new(engine), vec![]);
    let mut session = Session::new(d);
    let mut states = session.subscribe_state();

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    let mut seen = Vec::new();
    while let Ok(s) = states.try_recv() {
        seen.push(s);
    }
    assert_eq!(
        seen,
        vec![
            State::Connecting,
            State::Listening,
            State::Speaking,
            State::Listening
        ],
        "the HUD must see the whole transition sequence, not just the last state"
    );
}

#[tokio::test]
async fn the_capture_level_is_published_for_the_meter() {
    // `uia://level` at ~30 Hz. Unlike state, latest-only is correct here:
    // a meter that renders a stale backlog is worse than one that skips.
    let loud = vec![i16::MAX / 2; 480];
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let (d, act_tx) = deps(Box::new(engine), vec![loud]);
    let mut session = Session::new(d);
    let level = session.subscribe_level();

    assert_eq!(*level.borrow(), 0.0, "level starts silent");

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    let observed = *level.borrow();
    assert!(
        (observed - 0.5).abs() < 0.01,
        "a constant half-scale frame is rms ~0.5, observed {observed}"
    );
}

#[tokio::test]
async fn the_run_loop_stops_when_the_audio_source_is_exhausted() {
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let (d, act_tx) = deps(Box::new(engine), vec![vec![1, 2, 3]]);
    let mut session = Session::new(d);

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    assert_eq!(session.run().await, RunOutcome::SourceExhausted);
}

#[tokio::test]
async fn a_terminal_error_stops_the_run_loop_instead_of_retry_looping() {
    // The single most-misdiagnosed failure mode: a bad API key retried forever
    // behind a spinner. MANUAL-TEST.md checks this explicitly.
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::Error(EngineError::Auth("invalid api key".into())),
    ]);
    let (d, act_tx) = deps(Box::new(engine), vec![]);
    let mut session = Session::new(d);

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);

    assert_eq!(session.run().await, RunOutcome::Terminal);
    assert_eq!(
        session.reconnect_attempts(),
        0,
        "a terminal error must never be counted as a reconnect attempt"
    );
    assert_eq!(session.state(), State::Idle);
}

#[tokio::test]
async fn an_engine_error_message_is_broadcast_for_the_hud_to_surface() {
    // The bug this guards against: a rejected `session.update` (e.g. a
    // misplaced field the server's schema doesn't accept) used to only ever
    // move a state enum — the actual error text from OpenAI was discarded,
    // so it never reached a devtools console or a log anyone was watching.
    //
    // `Transport`, not `Auth`: that's genuinely how `uia-openai`'s
    // `engine_error()` classifies an "Unknown parameter" rejection (it isn't
    // an auth/permission error), and `Transport` is non-terminal — so `run()`
    // reconnects forever here exactly as it would in production (this is the
    // "Connecting"/"Listening" loop the live bug actually presented as).
    // Race the error broadcast against `run()` instead of awaiting it.
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::Error(EngineError::Transport("Unknown parameter: 'voice'".into())),
    ]);
    let (d, act_tx) = deps(Box::new(engine), vec![]);
    let mut session = Session::new(d);
    let mut errors = session.subscribe_error();

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);

    tokio::select! {
        msg = errors.recv() => {
            assert_eq!(
                msg.unwrap(),
                "transport error: Unknown parameter: 'voice'"
            );
        }
        _ = session.run() => panic!(
            "run() returned; a non-terminal error must retry forever, not exit"
        ),
    }
}

/// Always fails `connect` itself with a terminal error — `FakeEngine` cannot
/// express this either, since its `connect()` always returns `Ok` and only
/// ever fails via a scripted `EngineEvent::Error` after connecting.
struct DoorLockedEngine;

#[async_trait::async_trait]
impl S2sEngine for DoorLockedEngine {
    fn id(&self) -> EngineId {
        EngineId::OpenAi
    }
    fn input_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(16_000)
    }
    fn output_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(24_000)
    }
    async fn connect(
        &mut self,
        _cfg: &SessionConfig,
        _tx: Sender<EngineEvent>,
    ) -> Result<(), EngineError> {
        Err(EngineError::Auth("invalid api key".into()))
    }
    async fn send_audio(&mut self, _frame: &[i16]) -> Result<(), EngineError> {
        Ok(())
    }
    async fn send_tool_result(
        &mut self,
        _id: ToolCallId,
        _result: ToolResult,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    async fn interrupt(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
    async fn close(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
}

#[tokio::test]
async fn a_terminal_connect_failure_is_broadcast_not_just_the_bare_outcome() {
    // The bug this guards against: an invalid API key rejected by `connect()`
    // itself (before any event stream exists) used to move the state machine
    // to Terminal and stop there — the actual "invalid api key" text was
    // never sent to `error_tx`, so nothing but the bare `Terminal` outcome
    // ever reached stderr or the HUD. Only `EngineEvent::Error` (a failure
    // *after* connecting) used to broadcast a message; a `connect()` failure
    // did not.
    let (d, act_tx) = deps(Box::new(DoorLockedEngine), vec![]);
    let mut session = Session::new(d);
    let mut errors = session.subscribe_error();

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);

    // Unlike the non-terminal case above, a terminal `connect()` failure
    // makes `run()` return almost immediately — racing it against
    // `errors.recv()` in a `select!` would be flaky, since `run()` can win
    // outright. The broadcast channel buffers the send regardless of whether
    // anything was awaiting it yet, so it is still there to observe after.
    assert_eq!(session.run().await, RunOutcome::Terminal);
    assert_eq!(
        errors.try_recv().unwrap(),
        "authentication failed: invalid api key"
    );
}

/// Fails its first `connect` transiently, then succeeds — the network-came-back
/// case. `FakeEngine` cannot express this: it replays one script per connect.
struct FlakyEngine {
    attempts: usize,
    /// Held for the same reason every real engine holds it: dropping the sender
    /// closes the event stream, which the run loop correctly reads as the
    /// engine having gone away.
    tx: Option<Sender<EngineEvent>>,
}

#[async_trait::async_trait]
impl S2sEngine for FlakyEngine {
    fn id(&self) -> EngineId {
        EngineId::OpenAi
    }
    fn input_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(16_000)
    }
    fn output_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(24_000)
    }
    async fn connect(
        &mut self,
        _cfg: &SessionConfig,
        tx: Sender<EngineEvent>,
    ) -> Result<(), EngineError> {
        self.attempts += 1;
        if self.attempts == 1 {
            return Err(EngineError::Transport("network unreachable".into()));
        }
        let _ = tx.send(EngineEvent::Ready).await;
        self.tx = Some(tx);
        Ok(())
    }
    async fn send_audio(&mut self, _frame: &[i16]) -> Result<(), EngineError> {
        Ok(())
    }
    async fn send_tool_result(
        &mut self,
        _id: ToolCallId,
        _result: ToolResult,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    async fn interrupt(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
    async fn close(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn a_transient_failure_reconnects_after_backoff_and_reaches_listening() {
    // `Backoff` landed in S6 and nothing called it until now. Paused time so
    // the real 500 ms base delay costs the suite nothing.
    let (d, act_tx) = deps(
        Box::new(FlakyEngine {
            attempts: 0,
            tx: None,
        }),
        vec![vec![1, 2, 3]],
    );
    let mut session = Session::new(d);
    let mut states = session.subscribe_state();

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    assert_eq!(session.run().await, RunOutcome::SourceExhausted);

    assert_eq!(session.reconnect_attempts(), 1);
    assert_eq!(session.state(), State::Listening);

    let mut seen = Vec::new();
    while let Ok(s) = states.try_recv() {
        seen.push(s);
    }
    assert_eq!(
        seen.last(),
        Some(&State::Listening),
        "the retry must actually reach Listening, not just re-enter Connecting: {seen:?}"
    );
}

#[tokio::test]
async fn shutting_down_the_activation_channel_before_activating_ends_the_loop() {
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let (d, act_tx) = deps(Box::new(engine), vec![]);
    let mut session = Session::new(d);

    drop(act_tx);
    assert_eq!(session.run().await, RunOutcome::ShutDown);
    assert_eq!(session.state(), State::Idle, "never connected");
}

#[tokio::test]
async fn the_executors_tools_are_declared_to_the_engine_on_connect() {
    // Without this the app has an MCP client, a router, and an engine that is
    // never told any tool exists — `SessionConfig::tools` stayed empty from S4
    // through S13 because every test built the config by hand. The manual
    // checklist's "asking for the MCP tool's data calls the tool" cannot pass
    // until the session actually declares them.
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let configs = engine.config_probe();
    let (activation, act_tx) = ChannelActivation::new();
    let executor =
        std::sync::Arc::new(FakeExecutor::new().with_result("clock.now", ToolResult::ok("12:00")));

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor,
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    session.set_system_prompt("You are Assistant.");

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    let cfg = configs.last().expect("the engine was never connected");
    let names: Vec<&str> = cfg.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["clock.now"]);
    assert_eq!(cfg.system_prompt.as_deref(), Some("You are Assistant."));
}

#[tokio::test]
async fn a_tool_server_that_cannot_be_listed_still_yields_a_working_session() {
    // Degrade, never block: an MCP server that is down must cost the user its
    // tools, not the ability to talk at all.
    struct DeadExecutor;
    #[async_trait::async_trait]
    impl uia_core::tools::ToolExecutor for DeadExecutor {
        async fn list_tools(
            &self,
        ) -> Result<Vec<uia_core::tools::ToolDescriptor>, uia_core::tools::ToolError> {
            Err(uia_core::tools::ToolError::Transport("down".into()))
        }
        async fn execute(
            &self,
            _n: &str,
            _a: serde_json::Value,
            _d: std::time::Duration,
        ) -> Result<ToolResult, uia_core::tools::ToolError> {
            Err(uia_core::tools::ToolError::Transport("down".into()))
        }
    }

    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let configs = engine.config_probe();
    let (activation, act_tx) = ChannelActivation::new();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(DeadExecutor),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    assert_eq!(
        session.state(),
        State::Listening,
        "the session must connect"
    );
    assert!(configs.last().expect("connected").tools.is_empty());
}

#[tokio::test]
async fn a_tool_server_that_never_answers_still_yields_a_working_session() {
    // The sibling above covers a server that is *down*, which answers promptly
    // with an error. This is the one that is wedged: connected, accepting the
    // request, never replying. `recall` above it in `connect()` has a deadline
    // for exactly this reason; `list_tools` needs one too, because a persona
    // switch runs the whole of `connect()` with the user waiting on it.
    struct HungExecutor;
    #[async_trait::async_trait]
    impl uia_core::tools::ToolExecutor for HungExecutor {
        async fn list_tools(
            &self,
        ) -> Result<Vec<uia_core::tools::ToolDescriptor>, uia_core::tools::ToolError> {
            std::future::pending().await
        }
        async fn execute(
            &self,
            _n: &str,
            _a: serde_json::Value,
            _d: std::time::Duration,
        ) -> Result<ToolResult, uia_core::tools::ToolError> {
            std::future::pending().await
        }
    }

    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let configs = engine.config_probe();
    let (activation, act_tx) = ChannelActivation::new();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(HungExecutor),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    session.set_tools_deadline(std::time::Duration::from_millis(50));

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), session.run())
        .await
        .expect("connect must not wait on a tool server forever");

    assert_eq!(
        session.state(),
        State::Listening,
        "the session must connect anyway"
    );
    assert!(
        configs.last().expect("connected").tools.is_empty(),
        "a server that never answered contributes no tools"
    );
}

#[tokio::test]
async fn transcription_is_off_unless_the_shell_asks_for_it() {
    // It costs latency and money on every exchange, so nothing but an explicit
    // request should turn it on.
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let configs = engine.config_probe();
    let (d, act_tx) = deps(Box::new(engine), vec![]);
    let mut session = Session::new(d);

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    assert!(
        !configs
            .last()
            .expect("the engine was never connected")
            .transcription,
        "transcription must stay opt-in"
    );
}

#[tokio::test]
async fn set_transcription_reaches_the_engine_config() {
    // The user's half of every exchange rides on this reaching `connect`.
    // Without it the engine transcribes only its own speech, and the stored
    // conversation comes out one-sided: every assistant reply, and not one
    // word the user said.
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let configs = engine.config_probe();
    let (d, act_tx) = deps(Box::new(engine), vec![]);
    let mut session = Session::new(d);
    session.set_transcription(true);

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    assert!(
        configs
            .last()
            .expect("the engine was never connected")
            .transcription,
        "the engine must be asked to transcribe the user"
    );
}
