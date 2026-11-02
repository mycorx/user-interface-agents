// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The real run loop — what `uia-app` drives, as opposed to the
//! deterministic `run_until_*` drivers the earlier session tests use.
//!
//! S14 exists because nothing before it owned "compose the whole thing and
//! actually run it": `connect()` was private, state transitions were
//! unobservable from outside the crate, and `Backoff` (S6) existed but nothing
//! ever called it. These tests pin all three.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use uia_core::activation::{ActivationEvent, ChannelActivation};
use uia_core::audio::{
    AudioError, AudioFormat, AudioSource, DetachedSource, FixtureSource, SinkProbe, VecSink,
    WaitingCell, WaitingFor,
};
use uia_core::engine::{
    EngineError, EngineEvent, EngineId, FakeEngine, S2sEngine, SessionConfig, ToolCallId,
};
use uia_core::memory::{MemoryContext, NullMemory};
use uia_core::session::{
    AudioOpener, AudioPair, AudioSwap, RunOutcome, Session, SessionDeps, State,
};
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
async fn the_assistants_playback_level_is_published_for_the_meter() {
    // `uia://level` at ~30 Hz. Unlike state, latest-only is correct here:
    // a meter that renders a stale backlog is worse than one that skips.
    //
    // The wave is the assistant's voice, so the level is what the sink is
    // playing. A quiet mic frame is still needed to drive the pump cycle.
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::SpeechStarted,
        EngineEvent::AudioChunk(vec![i16::MAX / 2; 480]),
    ]);
    let (d, act_tx) = deps(Box::new(engine), vec![vec![0; 480]]);
    let mut session = Session::new(d);
    let level = session.subscribe_level();

    assert_eq!(*level.borrow(), 0.0, "level starts silent");

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    let observed = *level.borrow();
    assert!(
        (observed - 0.5).abs() < 0.01,
        "a constant half-scale chunk is rms ~0.5, observed {observed}"
    );
}

#[tokio::test]
async fn the_users_own_voice_does_not_move_the_meter() {
    // The bug this pins: the meter used to be the capture RMS, so the wave
    // danced to the user's microphone instead of the assistant's voice.
    let loud = vec![i16::MAX / 2; 480];
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let (d, act_tx) = deps(Box::new(engine), vec![loud]);
    let mut session = Session::new(d);
    let level = session.subscribe_level();

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    assert_eq!(
        *level.borrow(),
        0.0,
        "a loud mic with nothing playing is silent"
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

#[tokio::test]
async fn the_transcription_language_reaches_the_engine_config() {
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let configs = engine.config_probe();
    let (d, act_tx) = deps(Box::new(engine), vec![]);
    let mut session = Session::new(d);
    session.set_transcription_language(Some("en".to_string()));

    act_tx.send(ActivationEvent::Show).await.unwrap();
    drop(act_tx);
    session.run().await;

    assert_eq!(
        configs
            .last()
            .expect("the engine was never connected")
            .transcription_language
            .as_deref(),
        Some("en")
    );
}

/// An engine with a whole turn already scripted, so everything the run loop
/// does between frames has work to do from the first pass.
fn an_engine_with_a_scripted_turn() -> FakeEngine {
    FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::SpeechStarted,
        EngineEvent::AudioChunk(vec![8_000; 480]),
        EngineEvent::SpeechEnded,
    ])
}

/// A session whose microphone is open but yields nothing -- a muted device, a
/// driver that stopped delivering. Every test below drives the real `run()`
/// loop and must finish well inside its timeout: a capture read that blocked
/// the loop would freeze everything the loop does between frames.
fn a_session_with_a_silent_microphone(
    engine: FakeEngine,
) -> (Session, SinkProbe, Sender<ActivationEvent>) {
    let sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
    let speaker = sink.probe();
    let (activation, act_tx) = ChannelActivation::new();
    let session = Session::new(SessionDeps {
        source: Box::new(DetachedSource::new(AudioFormat::mono_pcm16(16_000))),
        sink: Box::new(sink),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    (session, speaker, act_tx)
}

const RUN_LOOP_BUDGET: Duration = Duration::from_secs(5);

/// Polls `done` until it holds, yielding to the run loop between checks.
async fn until(mut done: impl FnMut() -> bool) {
    while !done() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn engine_audio_reaches_the_speaker_while_the_microphone_yields_nothing() {
    let (mut session, speaker, act_tx) =
        a_session_with_a_silent_microphone(an_engine_with_a_scripted_turn());
    act_tx.send(ActivationEvent::Show).await.unwrap();

    let played = tokio::time::timeout(RUN_LOOP_BUDGET, async {
        tokio::select! {
            outcome = session.run() => panic!("run() ended with {outcome:?}"),
            () = until(|| !speaker.written().is_empty()) => {}
        }
    })
    .await;

    assert!(
        played.is_ok(),
        "the engine's audio never reached the speaker"
    );
}

#[tokio::test]
async fn opening_settings_parks_a_session_whose_microphone_is_silent() {
    let engine = an_engine_with_a_scripted_turn();
    let closes = engine.close_probe();
    let (mut session, _speaker, act_tx) = a_session_with_a_silent_microphone(engine);
    let control = session.control();
    let mut states = session.subscribe_state();
    act_tx.send(ActivationEvent::Show).await.unwrap();

    let parked = tokio::time::timeout(RUN_LOOP_BUDGET, async {
        tokio::select! {
            outcome = session.run() => panic!("run() ended with {outcome:?}"),
            () = async {
                while states.recv().await.unwrap() != State::Listening {}
                control.request_disconnect();
                while states.recv().await.unwrap() != State::Idle {}
            } => {}
        }
    })
    .await;

    assert!(parked.is_ok(), "the session never parked");
    assert_eq!(closes.count(), 1, "parking must close the engine");
}

#[tokio::test]
async fn a_session_with_a_silent_microphone_still_rotates_before_the_provider_cap() {
    let engine = an_engine_with_a_scripted_turn();
    let configs = engine.config_probe();
    let (mut session, _speaker, act_tx) = a_session_with_a_silent_microphone(engine);
    // Equal to the rotation margin, so the session is due at once.
    session.set_max_session_duration(Some(Duration::from_secs(30)));
    act_tx.send(ActivationEvent::Show).await.unwrap();

    let rotated = tokio::time::timeout(RUN_LOOP_BUDGET, async {
        tokio::select! {
            outcome = session.run() => panic!("run() ended with {outcome:?}"),
            () = until(|| configs.count() >= 2) => {}
        }
    })
    .await;

    assert!(rotated.is_ok(), "the session never rotated");
}

#[tokio::test]
async fn the_idle_deadline_still_disconnects_a_session_whose_microphone_is_silent() {
    let (mut session, _speaker, act_tx) =
        a_session_with_a_silent_microphone(an_engine_with_a_scripted_turn());
    session.set_idle_deadline(Duration::from_millis(50));
    let control = session.control();
    act_tx.send(ActivationEvent::Show).await.unwrap();

    let dropped = tokio::time::timeout(RUN_LOOP_BUDGET, async {
        tokio::select! {
            outcome = session.run() => panic!("run() ended with {outcome:?}"),
            () = until(|| !control.connection_wanted()) => {}
        }
    })
    .await;

    assert!(dropped.is_ok(), "the idle deadline never fired");
}

/// Speaks only when told to, after `connect` has long returned -- the case
/// `FakeEngine` cannot express, since it queues its whole script inside
/// `connect`, where the run loop drains it before it ever waits on anything.
struct LateSpeakingEngine {
    tx: std::sync::Arc<std::sync::Mutex<Option<Sender<EngineEvent>>>>,
}

/// The test's half of a `LateSpeakingEngine`: pushes events down the stream
/// the engine was connected with.
#[derive(Clone)]
struct LateVoice(std::sync::Arc<std::sync::Mutex<Option<Sender<EngineEvent>>>>);

impl LateVoice {
    async fn say(&self, ev: EngineEvent) {
        let tx = self.0.lock().unwrap().clone().expect("not connected yet");
        tx.send(ev).await.unwrap();
    }
}

#[async_trait::async_trait]
impl S2sEngine for LateSpeakingEngine {
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
        let _ = tx.send(EngineEvent::Ready).await;
        *self.tx.lock().unwrap() = Some(tx);
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
async fn engine_audio_sent_after_the_session_is_waiting_reaches_the_speaker_while_the_microphone_yields_nothing()
 {
    let tx = std::sync::Arc::new(std::sync::Mutex::new(None));
    let voice = LateVoice(tx.clone());
    let (activation, act_tx) = ChannelActivation::new();
    let sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
    let speaker = sink.probe();
    let mut session = Session::new(SessionDeps {
        source: Box::new(DetachedSource::new(AudioFormat::mono_pcm16(16_000))),
        sink: Box::new(sink),
        engine: Box::new(LateSpeakingEngine { tx }),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    let mut states = session.subscribe_state();
    act_tx.send(ActivationEvent::Show).await.unwrap();

    // Shorter than the session's 50 ms poll tick, so the audio has to be
    // woken for by the engine's own stream: a loop that only noticed it on
    // its next tick, or never (blocked on the silent microphone), misses it.
    const SOONER_THAN_THE_POLL_TICK: Duration = Duration::from_millis(40);

    tokio::select! {
        outcome = session.run() => panic!("run() ended with {outcome:?}"),
        () = async {
            while states.recv().await.unwrap() != State::Listening {}
            voice.say(EngineEvent::SpeechStarted).await;
            voice.say(EngineEvent::AudioChunk(vec![8_000; 480])).await;
            let played = tokio::time::timeout(
                SOONER_THAN_THE_POLL_TICK,
                until(|| !speaker.written().is_empty()),
            )
            .await;
            assert!(
                played.is_ok(),
                "engine audio sent while the session waited never reached the speaker in time"
            );
        } => {}
    }
}

/// A fresh speaker and the probe that watches it, as an opener would hand
/// them over.
fn a_fresh_pair() -> (AudioPair, SinkProbe) {
    let sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
    let probe = sink.probe();
    (
        (
            Box::new(DetachedSource::new(AudioFormat::mono_pcm16(16_000))),
            Box::new(sink),
        ),
        probe,
    )
}

#[tokio::test]
async fn an_audio_swap_requested_while_parked_applies_before_the_reconnect() {
    let engine = an_engine_with_a_scripted_turn();
    let configs = engine.config_probe();
    let (mut session, _old_speaker, act_tx) = a_session_with_a_silent_microphone(engine);
    let control = session.control();
    let mut states = session.subscribe_state();
    act_tx.send(ActivationEvent::Show).await.unwrap();

    let (pair, new_speaker) = a_fresh_pair();
    let connects_at_open = Arc::new(Mutex::new(None));

    let swapped = tokio::time::timeout(RUN_LOOP_BUDGET, async {
        tokio::select! {
            outcome = session.run() => panic!("run() ended with {outcome:?}"),
            () = async {
                while states.recv().await.unwrap() != State::Listening {}
                control.request_disconnect();
                while states.recv().await.unwrap() != State::Idle {}

                let seen = connects_at_open.clone();
                let configs = configs.clone();
                control.request_audio_swap(AudioSwap {
                    open: Some(Box::new(move || {
                        *seen.lock().unwrap() = Some(configs.count());
                        Ok(pair)
                    })),
                    barge_in_threshold: None,
                });
                control.request_connect();
                until(|| !new_speaker.written().is_empty()).await;
            } => {}
        }
    })
    .await;

    assert!(swapped.is_ok(), "the new speaker never played");
    assert_eq!(
        *connects_at_open.lock().unwrap(),
        Some(1),
        "the pair must open while parked, before the reconnect"
    );
}

/// Run a session whose first audio swap fails through `failing`, and check
/// it reports, keeps talking on a silent pair, and takes a later swap.
async fn a_failed_swap_is_survived(failing: AudioOpener, cause: &str) {
    let (mut session, _speaker, act_tx) =
        a_session_with_a_silent_microphone(an_engine_with_a_scripted_turn());
    let control = session.control();
    let mut errors = session.subscribe_error();
    let mut states = session.subscribe_state();
    control.request_audio_swap(AudioSwap {
        open: Some(failing),
        barge_in_threshold: None,
    });
    act_tx.send(ActivationEvent::Show).await.unwrap();

    let reopened = Arc::new(AtomicBool::new(false));
    let survived = tokio::time::timeout(RUN_LOOP_BUDGET, async {
        tokio::select! {
            outcome = session.run() => panic!("run() ended with {outcome:?}"),
            () = async {
                let message = errors.recv().await.unwrap();
                assert!(
                    message.contains("audio devices") && message.contains(cause),
                    "unexpected error: {message}"
                );
                while states.recv().await.unwrap() != State::Listening {}

                let flag = reopened.clone();
                control.request_audio_swap(AudioSwap {
                    open: Some(Box::new(move || {
                        flag.store(true, Ordering::SeqCst);
                        Ok(a_fresh_pair().0)
                    })),
                    barge_in_threshold: None,
                });
                until(|| reopened.load(Ordering::SeqCst)).await;
            } => {}
        }
    })
    .await;

    assert!(
        survived.is_ok(),
        "the session did not survive the failed swap"
    );
}

#[tokio::test]
async fn a_failed_or_panicking_opener_leaves_a_silent_pair_and_reports_the_error() {
    a_failed_swap_is_survived(
        Box::new(|| Err(AudioError::DeviceUnavailable("gone".into()))),
        "gone",
    )
    .await;
    a_failed_swap_is_survived(
        Box::new(|| panic!("the driver fell over")),
        "the driver fell over",
    )
    .await;
}

/// A failed opener leaves the session silent, so the failure is a held
/// state, not a one-shot error: it stays on the waiting line across a
/// reconnect (where the HUD clears its error line), the handle says the last
/// swap failed so the shell can retry an unchanged Save, and the next
/// successful swap clears both.
#[tokio::test]
async fn a_failed_opener_stays_published_until_a_swap_succeeds() {
    let (mut session, _speaker, act_tx) =
        a_session_with_a_silent_microphone(an_engine_with_a_scripted_turn());
    let control = session.control();
    let mut waiting = session.subscribe_audio_waiting();
    let mut states = session.subscribe_state();
    control.request_audio_swap(AudioSwap {
        open: Some(Box::new(|| {
            Err(AudioError::DeviceUnavailable("no default device".into()))
        })),
        barge_in_threshold: None,
    });
    act_tx.send(ActivationEvent::Show).await.unwrap();
    let failed = |w: &Option<String>| {
        w.as_deref()
            .is_some_and(|m| m.contains("audio devices") && m.contains("no default device"))
    };

    let held = tokio::time::timeout(RUN_LOOP_BUDGET, async {
        tokio::select! {
            outcome = session.run() => panic!("run() ended with {outcome:?}"),
            () = async {
                while states.recv().await.unwrap() != State::Listening {}
                assert!(failed(&waiting.borrow()), "got {:?}", *waiting.borrow());
                assert!(control.audio_swap_failed());

                control.request_disconnect();
                while states.recv().await.unwrap() != State::Idle {}
                control.request_connect();
                while states.recv().await.unwrap() != State::Listening {}
                assert!(
                    failed(&waiting.borrow()),
                    "the failure must outlive a reconnect, got {:?}",
                    *waiting.borrow()
                );
                assert!(control.audio_swap_failed());

                control.request_audio_swap(AudioSwap {
                    open: Some(Box::new(|| Ok(a_fresh_pair().0))),
                    barge_in_threshold: None,
                });
                waiting.wait_for(Option::is_none).await.unwrap();
                assert!(!control.audio_swap_failed());
            } => {}
        }
    })
    .await;

    assert!(held.is_ok(), "the failure was not held, or never cleared");
}

/// A microphone that never delivers and says it is waiting through a cell
/// the test keeps.
struct WaitingMicrophone(WaitingCell);

#[async_trait::async_trait]
impl AudioSource for WaitingMicrophone {
    fn format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(16_000)
    }
    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        std::future::pending().await
    }
    fn waiting(&self) -> Option<WaitingCell> {
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn a_waiting_microphone_is_published_naming_the_device() {
    let cell = WaitingCell::default();
    let (activation, act_tx) = ChannelActivation::new();
    let mut session = Session::new(SessionDeps {
        source: Box::new(WaitingMicrophone(cell.clone())),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(an_engine_with_a_scripted_turn()),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    let mut waiting = session.subscribe_audio_waiting();
    let mut states = session.subscribe_state();
    act_tx.send(ActivationEvent::Show).await.unwrap();

    let published = tokio::time::timeout(RUN_LOOP_BUDGET, async {
        tokio::select! {
            outcome = session.run() => panic!("run() ended with {outcome:?}"),
            () = async {
                while states.recv().await.unwrap() != State::Listening {}
                cell.set(Some(WaitingFor::Named("Jabra".into())));
                waiting
                    .wait_for(|w| w.as_deref() == Some("Waiting for the microphone \"Jabra\""))
                    .await
                    .unwrap();
                cell.set(None);
                waiting.wait_for(Option::is_none).await.unwrap();
            } => {}
        }
    })
    .await;

    assert!(
        published.is_ok(),
        "the wait was never published, or never cleared"
    );
}
