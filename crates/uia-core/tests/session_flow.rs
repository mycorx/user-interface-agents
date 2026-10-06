// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use uia_core::activation::{ActivationEvent, ChannelActivation};
use uia_core::audio::{AudioFormat, FixtureSource, VecSink};
use uia_core::engine::{EngineError, EngineEvent, FakeEngine, TextTurnSupport};
use uia_core::memory::{MemoryContext, NullMemory};
use uia_core::session::{Session, SessionDeps, State, SwitchError};
use uia_core::tools::FakeExecutor;

fn ctx() -> MemoryContext {
    MemoryContext {
        user_id: "test-user".into(),
        conversation_id: "test-convo".into(),
    }
}

#[tokio::test]
async fn activation_connects_the_engine_and_reaches_listening() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    assert_eq!(session.state(), State::Listening);
}

#[tokio::test]
async fn captured_audio_reaches_the_engine() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let probe = engine.audio_probe();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![vec![10, 20], vec![30, 40]],
        )),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_source_exhausted().await;

    assert_eq!(probe.sent(), vec![10, 20, 30, 40]);
}

#[tokio::test]
async fn gated_capture_stops_frames_reaching_the_engine() {
    // The mirror of the test above, and the whole point of `SessionControl`:
    // same source, same engine, gate closed — the engine must see nothing.
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let probe = engine.audio_probe();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![vec![10, 20], vec![30, 40]],
        )),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    let control = session.control();
    control.set_capture_enabled(false);

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_source_exhausted().await;

    assert!(
        probe.sent().is_empty(),
        "muted capture still reached the engine: {:?}",
        probe.sent()
    );
    // Gating forwarding is not deactivating: the session is still connected
    // and listening (FD6).
    assert_eq!(session.state(), State::Listening);
}

#[tokio::test]
async fn ungating_capture_resumes_forwarding_mid_session() {
    // A gate that could only ever be closed would pass the test above while
    // being useless. Frames 1 and 2 are dropped, frames 3 and 4 are not.
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let probe = engine.audio_probe();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![vec![10, 20], vec![30, 40], vec![50, 60], vec![70, 80]],
        )),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    let control = session.control();
    control.set_capture_enabled(false);

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    session.pump_audio_once().await;
    session.pump_audio_once().await;
    assert!(probe.sent().is_empty(), "gate leaked while closed");

    control.set_capture_enabled(true);
    session.pump_audio_once().await;
    session.pump_audio_once().await;

    assert_eq!(probe.sent(), vec![50, 60, 70, 80]);
}

#[tokio::test]
async fn gated_capture_reports_a_flat_level_and_never_barges_in() {
    // A loud frame is the one that would both move the meter and interrupt
    // the assistant. Muted, it must do neither: an equalizer that keeps
    // dancing while the mic is off says "you are being heard", and an
    // interrupt from audio the engine never received is worse still.
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready, EngineEvent::SpeechStarted]);
    let probe = engine.audio_probe();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![vec![i16::MAX; 32]],
        )),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    let control = session.control();
    let mut level = session.subscribe_level();
    control.set_capture_enabled(false);

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Speaking).await;
    assert_eq!(session.state(), State::Speaking);

    session.pump_audio_once().await;

    assert_eq!(*level.borrow_and_update(), 0.0);
    assert_eq!(session.state(), State::Speaking, "muted audio barged in");
    assert!(probe.sent().is_empty());
}

#[tokio::test]
async fn a_typed_turn_reaches_the_engine_through_the_control_handle() {
    // PLAN.md S4: the typed-turn command doesn't talk to the engine
    // directly — it submits through the same control handle S2 built for
    // the mic gate, and the run loop is what actually calls `send_text`.
    // `FakeEngine` doesn't override `send_text`, so it inherits
    // `S2sEngine`'s default "not supported" error — which is exactly the
    // behaviour this asserts: the failure is surfaced on `error_tx`, not
    // swallowed, and it must NOT be treated as a connection problem.
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    let control = session.control();
    let mut errors = session.subscribe_error();

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;
    assert_eq!(session.state(), State::Listening);

    control.submit_text_turn("what's the weather?").unwrap();
    assert!(
        session.pump_text_once().await,
        "the queued turn was dropped"
    );

    let msg = errors.try_recv().expect("the failed send must be reported");
    assert!(msg.to_lowercase().contains("not supported"), "got: {msg}");
    assert_eq!(
        session.state(),
        State::Listening,
        "an engine's own text-support answer is not a connection problem"
    );
    assert_eq!(
        session.reconnect_attempts(),
        0,
        "a typed-turn failure must not trigger reconnect backoff"
    );
}

#[tokio::test]
async fn a_failed_typed_turn_degrades_this_session_to_unsupported_and_stays_there() {
    // PLAN.md FD3: the capability an engine *claims* is an opening bid; a
    // turn it actually refused is the answer. The demotion happens once, in
    // the backend, so every surface reading the handle follows without
    // re-implementing the rule — and it is sticky, because a capability that
    // flapped back to `Unknown` would re-enable a control now known to fail.
    let (activation, act_tx) = ChannelActivation::new();
    // `FakeEngine` overrides neither `send_text` nor `text_turn_support`, so
    // it starts on the trait defaults: `Unknown` (a claim never made) and a
    // `send_text` that errors.
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    let control = session.control();
    let mut support = control.subscribe_text_turn_support();

    assert_eq!(
        session.text_turn_support(),
        TextTurnSupport::Unknown,
        "an engine that made no claim must not be pre-emptively disabled"
    );
    assert!(session.text_turn_support().is_usable());

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    control.submit_text_turn("what's the weather?").unwrap();
    assert!(
        session.pump_text_once().await,
        "the queued turn was dropped"
    );

    let after = session.text_turn_support();
    assert!(
        !after.is_usable(),
        "a refused turn must demote the capability, got: {after:?}"
    );
    // The reason carried is the failure itself, not a generic string: it is
    // what the HUD shows the user (FD6).
    let reason = after.reason().expect("Unsupported carries a reason");
    assert!(
        reason.to_lowercase().contains("not supported"),
        "got: {reason}"
    );

    // The shell learns about it through the handle, not by polling.
    assert!(
        support.has_changed().unwrap(),
        "a degrade must wake a subscriber"
    );
    assert_eq!(*support.borrow_and_update(), after);

    // Sticky: reading again, and pumping an empty queue, must not restore it.
    assert!(!session.pump_text_once().await);
    assert_eq!(
        session.text_turn_support(),
        after,
        "the degrade must survive a subsequent read"
    );
    assert_eq!(control.text_turn_support(), after);
}

#[tokio::test]
async fn engine_audio_chunks_reach_the_sink() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::SpeechStarted,
        EngineEvent::AudioChunk(vec![5, 6, 7]),
        EngineEvent::SpeechEnded,
    ]);
    let sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
    let sink_probe = sink.probe();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(sink),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    assert_eq!(sink_probe.written(), vec![5, 6, 7]);
}

#[tokio::test]
async fn engine_audio_is_resampled_to_the_sinks_rate() {
    // FakeEngine speaks at 24 kHz (matching OpenAI's real output_format). A
    // sink at a different rate — a typical Windows default output device
    // negotiates 48 kHz — must never receive that audio unresampled: writing
    // 24 kHz samples into a 48 kHz playback stream plays them back at double
    // speed and pitch, which is the "bad mechanical synthesized voice" a rate
    // mismatch actually sounds like.
    //
    // Multiple chunks, as a real engine streams them (~20 ms each): the
    // resampler's FFT filter needs a full internal block before it emits
    // anything, so one isolated chunk legitimately yields nothing yet — see
    // `resampled_capture_is_continuous_across_frames` for the same reasoning
    // on the capture side.
    const CHUNK: usize = 480;
    const CHUNKS: usize = 30;
    let tone: Vec<i16> = (0..CHUNK * CHUNKS)
        .map(|n| {
            let t = n as f64 / 24_000.0;
            ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8000.0) as i16
        })
        .collect();

    let (activation, act_tx) = ChannelActivation::new();
    let mut events = vec![EngineEvent::Ready, EngineEvent::SpeechStarted];
    events.extend(
        tone.chunks(CHUNK)
            .map(|c| EngineEvent::AudioChunk(c.to_vec())),
    );
    events.push(EngineEvent::SpeechEnded);
    let engine = FakeEngine::new(events);
    let sink = VecSink::new(AudioFormat::mono_pcm16(48_000));
    let sink_probe = sink.probe();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(sink),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    let written = sink_probe.written();
    // 24 kHz -> 48 kHz is a 2x ratio; allow slack for the resampler's filter
    // warm-up rather than pin an exact sample count.
    let expected = CHUNK * CHUNKS * 2;
    assert!(
        written.len().abs_diff(expected) < expected / 5,
        "expected roughly {expected} samples at 48 kHz, got {}",
        written.len()
    );
}

#[tokio::test]
async fn user_speech_during_playback_clears_the_sink_locally() {
    let (activation, act_tx) = ChannelActivation::new();
    // The fake NEVER emits Interrupted, proving we do not wait for the network.
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::SpeechStarted,
        EngineEvent::AudioChunk(vec![100; 480]),
    ]);
    let interrupts = engine.interrupt_probe();
    let sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
    let sink_probe = sink.probe();

    // A loud frame: well above the default threshold.
    let loud = vec![20_000i16; 320];

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![loud],
        )),
        sink: Box::new(sink),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Speaking).await;
    assert!(
        !sink_probe.written().is_empty(),
        "precondition: playback started"
    );

    session.pump_audio_once().await;

    assert_eq!(
        sink_probe.written(),
        Vec::<i16>::new(),
        "sink must be cleared"
    );
    assert_eq!(sink_probe.clear_count(), 1);
    assert_eq!(session.state(), State::Interrupting);
    assert_eq!(
        interrupts.count(),
        1,
        "engine.interrupt() still sent, but after clear()"
    );
}

#[tokio::test]
async fn quiet_input_during_playback_does_not_trigger_barge_in() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::SpeechStarted,
        EngineEvent::AudioChunk(vec![100; 480]),
    ]);
    let sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
    let sink_probe = sink.probe();
    let quiet = vec![5i16; 320];

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![quiet],
        )),
        sink: Box::new(sink),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Speaking).await;
    session.pump_audio_once().await;

    assert_eq!(sink_probe.clear_count(), 0);
    assert_eq!(session.state(), State::Speaking);
}

/// Ordinary speech sits in the band between the old 0.05 default and the
/// tuned one. Under 0.05 a frame at this level did not cross the threshold,
/// so barge-in waited for the speaker to build to a louder peak before any
/// frame triggered it -- the audible delay the SP2 checklist measured as a
/// ~1s barge-in gap. Anything above the tuned default must cut immediately.
#[tokio::test]
async fn speech_above_the_tuned_default_triggers_barge_in_immediately() {
    let (activation, act_tx) = ChannelActivation::new();
    // The fake NEVER emits Interrupted, proving we do not wait for the network.
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::SpeechStarted,
        EngineEvent::AudioChunk(vec![100; 480]),
    ]);
    let interrupts = engine.interrupt_probe();
    let sink = VecSink::new(AudioFormat::mono_pcm16(24_000));
    let sink_probe = sink.probe();
    // RMS ~0.04: over the tuned 0.03 default, under the old 0.05.
    let speech = vec![1_311i16; 320];

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![speech],
        )),
        sink: Box::new(sink),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Speaking).await;
    assert!(
        !sink_probe.written().is_empty(),
        "precondition: playback started"
    );

    session.pump_audio_once().await;

    assert_eq!(
        sink_probe.clear_count(),
        1,
        "speech at this level must barge in on the first frame"
    );
    assert_eq!(session.state(), State::Interrupting);
    assert_eq!(interrupts.count(), 1);
}

#[tokio::test]
async fn a_tool_call_is_executed_and_its_result_returned_to_the_engine() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::ToolCall {
            id: "call-1".into(),
            name: "clock.now".into(),
            args: serde_json::json!({"tz": "UTC"}),
        },
    ]);
    let results = engine.tool_result_probe();
    let executor = std::sync::Arc::new(
        FakeExecutor::new().with_result("clock.now", uia_core::tools::ToolResult::ok("12:00")),
    );

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: executor.clone(),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Thinking).await;

    assert_eq!(executor.calls().len(), 1);
    assert_eq!(executor.calls()[0].0, "clock.now");
    let sent = results.results();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "call-1");
    assert_eq!(sent[0].1.content, "12:00");
    assert!(!sent[0].1.is_error);
}

#[tokio::test]
async fn a_hung_tool_times_out_and_the_session_survives() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::ToolCall {
            id: "call-slow".into(),
            name: "slow.tool".into(),
            args: serde_json::json!({}),
        },
    ]);
    let results = engine.tool_result_probe();
    let executor = std::sync::Arc::new(
        FakeExecutor::new()
            .with_result("slow.tool", uia_core::tools::ToolResult::ok("never"))
            .with_delay(std::time::Duration::from_secs(60)),
    );

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor,
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    session.set_tool_deadline(std::time::Duration::from_millis(50));

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Thinking).await;

    let sent = results.results();
    assert_eq!(sent.len(), 1, "an error result must still be sent");
    assert!(sent[0].1.is_error, "result must be flagged as an error");
    assert!(
        sent[0].1.content.contains("timed out"),
        "got: {}",
        sent[0].1.content
    );
    assert_eq!(session.state(), State::Thinking, "session must not be dead");
}

#[tokio::test]
async fn a_terminal_auth_error_does_not_reconnect() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::Error(EngineError::Auth("bad key".into())),
    ]);

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    // `run_until_idle_or` already drains every buffered event to quiescence
    // (see its doc comment), so it settles in `Idle` on its own once the
    // terminal error fires — no need for a further blocking pump. Doing so
    // would deadlock: `FakeEngine` keeps its `Sender` alive in `self.tx`, so
    // the channel never disconnects and `pump_engine`'s `recv().await` waits
    // forever once the script is exhausted (the same hazard S4's ledger notes
    // flag for `run_until(predicate)`-style loops against this fake).
    session.run_until_idle_or(State::Listening).await;

    assert_eq!(
        session.state(),
        State::Idle,
        "auth failure must be terminal"
    );
    assert_eq!(
        session.reconnect_attempts(),
        0,
        "must NOT retry-loop on auth"
    );
}

#[tokio::test]
async fn a_transient_error_moves_to_connecting_for_retry() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::Error(EngineError::Transport("reset".into())),
    ]);

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    // Target `Connecting` directly: that's where the scripted `Ready` then
    // transient `Error` settles once fully drained, and `run_until_idle_or`
    // only asserts `target` or `Idle` — see the auth test above for why no
    // further blocking pump follows.
    session.run_until_idle_or(State::Connecting).await;

    assert_eq!(session.state(), State::Connecting);
    assert_eq!(session.reconnect_attempts(), 1);
}

#[tokio::test]
async fn switching_engines_is_allowed_while_idle() {
    let (activation, _tx) = ChannelActivation::new();
    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(FakeEngine::new(vec![])),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    assert_eq!(session.state(), State::Idle);
    assert!(
        session
            .switch_engine(Box::new(FakeEngine::new(vec![])))
            .is_ok()
    );
}

#[tokio::test]
async fn switching_engines_is_rejected_while_speaking() {
    let (activation, act_tx) = ChannelActivation::new();
    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(FakeEngine::new(vec![
            EngineEvent::Ready,
            EngineEvent::SpeechStarted,
        ])),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Speaking).await;
    assert_eq!(session.state(), State::Speaking);

    let err = session.switch_engine(Box::new(FakeEngine::new(vec![])));
    assert!(matches!(err, Err(SwitchError::NotIdle(State::Speaking))));
}

#[tokio::test]
async fn recall_results_are_injected_into_the_session_config() {
    use uia_core::memory::{MemoryItem, MemoryKind, SlowMemory};

    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let cfg_probe = engine.config_probe();

    let memory = std::sync::Arc::new(SlowMemory::instant(vec![MemoryItem {
        content: "prefers metric units".into(),
        kind: MemoryKind::Preference,
        score: Some(0.9),
    }]));

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory,
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    let cfg = cfg_probe.last().expect("engine must have been connected");
    assert_eq!(cfg.memory_context.len(), 1);
    assert_eq!(cfg.memory_context[0].content, "prefers metric units");
}

#[tokio::test]
async fn slow_recall_times_out_and_the_session_connects_without_memory() {
    use uia_core::memory::SlowMemory;

    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let cfg_probe = engine.config_probe();

    // Far slower than the deadline this test sets below.
    let memory = std::sync::Arc::new(SlowMemory::delayed(
        std::time::Duration::from_secs(30),
        vec![],
    ));

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory,
        activation: Box::new(activation),
        ctx: ctx(),
    });
    session.set_recall_deadline(std::time::Duration::from_millis(50));

    let started = std::time::Instant::now();
    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;
    let elapsed = started.elapsed();

    assert_eq!(
        session.state(),
        State::Listening,
        "memory must never gate connecting"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "took {elapsed:?}"
    );
    assert!(cfg_probe.last().unwrap().memory_context.is_empty());
}

#[tokio::test]
async fn resampled_capture_is_continuous_across_frames() {
    // The capture path resamples one frame at a time. A resampler rebuilt per
    // frame both loses filter state at every boundary and -- once the filter is
    // stateful -- swallows frames smaller than its chunk size entirely, so the
    // engine hears nothing at all. Pumping N frames must equal resampling the
    // whole stream once.
    use uia_core::audio::Resampler;

    const FRAME: usize = 480;
    const FRAMES: usize = 30;

    let signal: Vec<i16> = (0..FRAME * FRAMES)
        .map(|n| {
            let t = n as f64 / 48_000.0;
            ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8000.0) as i16
        })
        .collect();
    let frames: Vec<Vec<i16>> = signal.chunks(FRAME).map(<[i16]>::to_vec).collect();

    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let probe = engine.audio_probe();

    let mut session = Session::new(SessionDeps {
        // 48 kHz source against FakeEngine's 16 kHz input format.
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(48_000), frames)),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;
    for _ in 0..FRAMES {
        session.pump_audio_once().await;
    }

    let mut reference = Resampler::new(48_000, 16_000, 1024).unwrap();
    let expected = reference.process(&signal).unwrap();

    assert!(
        !expected.is_empty(),
        "the reference resample produced nothing; the fixture is wrong"
    );
    assert_eq!(
        probe.sent(),
        expected,
        "engine received {} samples, one-shot resampling gives {}",
        probe.sent().len(),
        expected.len()
    );
}

#[tokio::test]
async fn disconnect_closes_the_engine_and_returns_to_idle() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let closes = engine.close_probe();

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;
    assert_eq!(session.state(), State::Listening);

    session.disconnect().await;
    assert_eq!(session.state(), State::Idle);
    assert_eq!(
        closes.count(),
        1,
        "disconnect must actually close the engine"
    );

    // Idempotent: the shell can ask twice (Settings reopened, an idle timer
    // firing while a disconnect is already in flight) and the second ask must
    // be a no-op rather than an error or a second close.
    session.disconnect().await;
    assert_eq!(session.state(), State::Idle);
    assert_eq!(
        closes.count(),
        1,
        "a second disconnect must not close again"
    );
}

#[tokio::test]
async fn disconnecting_collapses_the_meter_so_a_deaf_window_never_looks_live() {
    // The rotation path — a persona switch, the max-session-duration
    // rotation, every reconnect — disconnects before it reconnects, and for
    // the whole gap speech is discarded rather than buffered. The run loop is
    // parked inside `connect()` for that window, so `pump_audio` is not
    // called and the meter simply holds its last value: a ribbon frozen
    // mid-shape, reading exactly as "you are being heard".
    //
    // This is the same honesty rule the capture gate already follows, for the
    // same reason it gives: "a muted mic that still animates reads as 'you
    // are being heard'". A deaf session is not different in kind from a muted
    // one — in both, the level the meter is showing is going nowhere.
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);
    let loud = vec![i16::MAX / 2; 480];

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![loud.clone(), loud.clone(), loud],
        )),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });
    let level = session.subscribe_level();

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;
    session.pump_audio_once().await;

    let live = *level.borrow();
    assert!(
        live > 0.4,
        "the meter must be live before the window, observed {live}"
    );

    session.disconnect().await;
    assert_eq!(
        *level.borrow(),
        0.0,
        "a session with no engine to send audio to must not leave a level standing"
    );
}

#[tokio::test]
async fn a_disconnected_session_reconnects_without_being_rebuilt() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready, EngineEvent::Ready]);

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    session.disconnect().await;
    assert_eq!(session.state(), State::Idle);

    // The point of the whole change: the session object is still alive, still
    // holds its audio devices, and connects again with nothing rebuilt.
    session.run_until_idle_or(State::Listening).await;
    assert_eq!(session.state(), State::Listening);
}

#[tokio::test]
async fn a_zero_idle_deadline_disables_the_idle_disconnect() {
    let (activation, _act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![EngineEvent::Ready]);

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: std::sync::Arc::new(NullMemory),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    session.set_idle_deadline(std::time::Duration::ZERO);
    // Nothing to assert beyond "this is accepted and does not fire": the
    // opt-out exists for a shell that would rather hold the connection open,
    // and a zero deadline must mean disabled, not "expire immediately".
    assert_eq!(session.state(), State::Idle);
}

/// A recording memory that captures what the session filed, so the test can
/// assert on turns without a filesystem.
#[derive(Default)]
struct RecordingMemory {
    turns: std::sync::Mutex<Vec<(Option<String>, Option<String>)>>,
}

#[async_trait::async_trait]
impl uia_core::memory::ConversationMemory for RecordingMemory {
    async fn recall(
        &self,
        _ctx: &uia_core::memory::MemoryContext,
        _q: Option<&str>,
    ) -> Result<Vec<uia_core::memory::MemoryItem>, uia_core::memory::MemoryError> {
        Ok(vec![])
    }
    async fn record(
        &self,
        _ctx: &uia_core::memory::MemoryContext,
        turn: &uia_core::memory::Turn,
    ) -> Result<(), uia_core::memory::MemoryError> {
        self.turns
            .lock()
            .unwrap()
            .push((turn.user.clone(), turn.assistant.clone()));
        Ok(())
    }
}

/// The bug this whole change exists to fix: `UserTranscript` and
/// `ModelTranscript` were dropped by an unreachable catch-all arm, so nothing
/// was ever recorded and every reconnect resumed blank. The engines had always
/// been sending them.
#[tokio::test]
async fn transcripts_are_recorded_as_a_turn_when_the_turn_completes() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::UserTranscript("what is the capital of France".into()),
        EngineEvent::SpeechStarted,
        EngineEvent::ModelTranscript("Paris".into()),
        EngineEvent::SpeechEnded,
        EngineEvent::TurnComplete,
    ]);
    let memory = std::sync::Arc::new(RecordingMemory::default());

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: memory.clone(),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    let recorded = memory.turns.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "one exchange, not one per fragment");
    assert_eq!(
        recorded[0].0.as_deref(),
        Some("what is the capital of France")
    );
    assert_eq!(recorded[0].1.as_deref(), Some("Paris"));
}

/// A `SpeechEnded` carrying no transcript at all must not file a blank entry —
/// the session flushes on every one of them.
#[tokio::test]
async fn a_silent_exchange_records_nothing() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::SpeechStarted,
        EngineEvent::SpeechEnded,
    ]);
    let memory = std::sync::Arc::new(RecordingMemory::default());

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: memory.clone(),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    assert!(memory.turns.lock().unwrap().is_empty());
}

/// The bug `TurnComplete` exists to fix, in the order it actually happened
/// live: OpenAI's `response.output_audio.done` arrived before
/// `response.output_audio_transcript.done`, so a flush at `SpeechEnded` wrote
/// the user's half alone and pushed the assistant's into the following
/// exchange. One spoken sentence produced two one-sided entries.
#[tokio::test]
async fn a_transcript_arriving_after_the_audio_stops_still_pairs_with_its_exchange() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::UserTranscript("what is the capital of France".into()),
        EngineEvent::SpeechStarted,
        // Audio stops BEFORE the transcript lands — the whole point.
        EngineEvent::SpeechEnded,
        EngineEvent::ModelTranscript("Paris".into()),
        EngineEvent::TurnComplete,
    ]);
    let memory = std::sync::Arc::new(RecordingMemory::default());

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: memory.clone(),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    let recorded = memory.turns.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        1,
        "one exchange, not a one-sided entry per half"
    );
    assert_eq!(
        recorded[0].0.as_deref(),
        Some("what is the capital of France"),
        "the user's half must not be flushed on its own"
    );
    assert_eq!(
        recorded[0].1.as_deref(),
        Some("Paris"),
        "a transcript arriving after the audio must still join its exchange"
    );
}

/// The assistant's playback bracket closing is not a turn boundary. Recording
/// there is what produced the off-by-one in the first place.
#[tokio::test]
async fn speech_ending_records_nothing_on_its_own() {
    let (activation, act_tx) = ChannelActivation::new();
    let engine = FakeEngine::new(vec![
        EngineEvent::Ready,
        EngineEvent::UserTranscript("what is the capital of France".into()),
        EngineEvent::SpeechStarted,
        EngineEvent::ModelTranscript("Paris".into()),
        EngineEvent::SpeechEnded,
    ]);
    let memory = std::sync::Arc::new(RecordingMemory::default());

    let mut session = Session::new(SessionDeps {
        source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
        sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        engine: Box::new(engine),
        executor: std::sync::Arc::new(FakeExecutor::new()),
        memory: memory.clone(),
        activation: Box::new(activation),
        ctx: ctx(),
    });

    act_tx.send(ActivationEvent::Show).await.unwrap();
    session.run_until_idle_or(State::Listening).await;

    assert!(
        memory.turns.lock().unwrap().is_empty(),
        "nothing may be written until the engine says the turn is complete"
    );
}
