// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

mod audio;
pub mod backoff;
pub mod control;
pub mod state;
pub use backoff::Backoff;
pub use control::{
    AudioOpener, AudioPair, AudioSwap, PromptSwap, PromptSwapOutcome, SessionControl,
};
pub use state::{SessionInput, State, can_switch_engine, transition};

use crate::activation::{Activation, ActivationEvent};
use crate::audio::{AudioSink, AudioSource, DetachedSink, DetachedSource, Resampler};
use crate::engine::{EngineEvent, S2sEngine, SessionConfig, TextTurnSupport};
use crate::memory::{ConversationMemory, MemoryContext};
use crate::tools::ToolExecutor;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SwitchError {
    #[error("cannot switch engine while in {0:?}; only Idle is permitted")]
    NotIdle(State),
}

/// Upper bound on pump iterations in the test drivers. Purely a hang guard: a
/// misbehaving engine should fail the suite, never wedge it.
const MAX_PUMPS: usize = 256;

/// How often the run loop wakes with nothing else to do: to publish the
/// playback level while the microphone is silent, and to re-run the checks
/// (rotation, the idle deadline) that only a pass of the loop performs.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Default RMS above which input counts as the user speaking over the
/// assistant.
///
/// Tuned on real Windows hardware against the SP2 WASAPI AEC checklist's
/// barge-in item, which measured ~1s between the user speaking over the
/// assistant and its audio actually stopping. The original 0.05 was never
/// tuned and sat above conversational speech at normal mic gain: the first
/// syllables of an interruption did not cross it, so barge-in waited for
/// the speaker to build to a louder peak before any frame triggered it.
/// That wait is the delay the checklist measured -- not the AEC framing,
/// and not the sink. 0.03 catches an interruption on its first frame.
///
/// Do not raise this without measuring. Barge-in only runs while the
/// assistant is speaking, so a value that also admits residual echo makes
/// the session interrupt itself; the margin between the two is what is
/// being tuned here, and it is mic-gain-dependent. Override per-machine
/// with `audio.barge_in_threshold` in `uia.toml` rather than moving this.
const DEFAULT_BARGE_IN_THRESHOLD: f32 = 0.03;

/// Default ceiling on a single tool execution before its result becomes a
/// spoken error rather than hanging the session.
const DEFAULT_TOOL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// How many recorded exchanges a slow subscriber may fall behind before the
/// oldest are dropped. Turns arrive at conversational pace -- seconds apart at
/// the very fastest -- so this is generous by construction; it exists so a
/// webview that never came up cannot pin them in memory forever.
const TURN_BROADCAST_CAPACITY: usize = 64;

/// How long a session may sit with nothing being said before it drops its
/// engine connection.
///
/// Chosen to fire well before providers drop an idle realtime session
/// themselves: being disconnected on our own terms is a state we can show and
/// recover from, whereas a remote close arrives as an error mid-session. Five
/// minutes is comfortably inside every provider's window while being long
/// enough that a pause for thought never costs a reconnect.
const DEFAULT_IDLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// How far ahead of the provider's ceiling to rotate a session.
///
/// Rotating early is the entire point: reaching the cap means the provider
/// closes the stream underneath a possibly mid-sentence exchange, and the user
/// sees an error. Thirty seconds is comfortably longer than a connect (~1s)
/// while being short enough not to waste much of the window -- on Bedrock's
/// eight minutes it costs about 6%.
const SESSION_ROTATION_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);

/// Default ceiling on memory recall before connecting proceeds without it.
/// Recall sits on the critical path to first audio, so it gets a hard budget:
/// memory is an enhancement and must never gate responding.
const DEFAULT_RECALL_DEADLINE: std::time::Duration = std::time::Duration::from_millis(500);

/// Ceiling on listing tools during `connect()`. Longer than the recall
/// deadline because tools are worth more waiting for, and because
/// `McpRouter` already time-boxes each server underneath this -- reaching
/// this outer bound means something below it failed to honour its own.
const DEFAULT_TOOLS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// How many state transitions the HUD may fall behind before it starts
/// missing them. Generous: a transition is two bytes and the whole point of a
/// broadcast here is that the sequence survives a slow renderer.
const STATE_BROADCAST_CAPACITY: usize = 64;

/// Errors are rare compared to state transitions, so a much smaller backlog
/// still comfortably outruns a lagging subscriber.
const ERROR_BROADCAST_CAPACITY: usize = 16;

/// Why the real run loop stopped. Every variant is a genuinely different thing
/// for the shell to do, which is why this is not a `bool` or a `Result<()>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The engine closed the stream. Reconnecting is the caller's decision.
    EngineClosed,
    /// The capture source ran out. A real device never does this; a fixture does.
    SourceExhausted,
    /// A terminal error — an invalid key, a rejected config. Deliberately NOT
    /// retried: an auth failure retried behind a spinner is the single
    /// most-misdiagnosed failure mode this project has (`EngineError::is_terminal`).
    Terminal,
    /// The activation channel closed: the application is shutting down.
    ShutDown,
}

pub struct SessionDeps {
    pub source: Box<dyn AudioSource>,
    pub sink: Box<dyn AudioSink>,
    pub engine: Box<dyn S2sEngine>,
    pub executor: Arc<dyn ToolExecutor>,
    pub memory: Arc<dyn ConversationMemory>,
    pub activation: Box<dyn Activation>,
    pub ctx: MemoryContext,
}

pub struct Session {
    /// `deps.source` is a placeholder once `new` returns: the real source
    /// lives in `capture`, which reads it on a task of its own.
    deps: SessionDeps,
    /// The microphone, read on its own task so a source that yields nothing
    /// never stalls the run loop (`session/audio.rs`).
    capture: audio::Capture,
    state: State,
    events: Option<mpsc::Receiver<EngineEvent>>,
    /// An engine event the run loop's `select!` received while waiting.
    /// Handled by the next drain ahead of anything still queued, so waiting
    /// for an event never reorders it.
    stashed_event: Option<EngineEvent>,
    /// Typed turns (PLAN.md S4) waiting to be sent. Taken from `control`
    /// whenever `control` changes (construction, `set_control`) so the
    /// per-pump drain never has to lock — see `control.rs`'s take-once
    /// handoff.
    text_rx: Option<mpsc::Receiver<String>>,
    barge_in_threshold: f32,
    tool_deadline: std::time::Duration,
    recall_deadline: std::time::Duration,
    /// Ceiling on `list_tools()` during `connect()`. Its own deadline
    /// rather than sharing `recall_deadline`: losing recall costs the
    /// session its memory, while losing tools costs it every capability
    /// including `switch_persona`, so the two are not worth the same wait.
    tools_deadline: std::time::Duration,
    /// Disconnect after this long with no speech in either direction.
    idle_deadline: std::time::Duration,
    /// Last time anything was actually said -- audio forwarded to the engine,
    /// or an event received from it. `None` while disconnected, so a parked
    /// session is never judged idle.
    last_activity: Option<std::time::Instant>,
    /// When the current connection was established, for the per-provider
    /// maximum-session-duration cap. `None` while disconnected.
    connected_at: Option<std::time::Instant>,
    /// Providers close a realtime session after a fixed wall-clock lifetime
    /// regardless of activity -- 8 minutes on Bedrock Nova Sonic, 30 on Azure,
    /// 60 on OpenAI. Rotating just before that is what turns a mid-sentence
    /// remote close into an invisible reconnect.
    max_session_duration: Option<std::time::Duration>,
    /// Transcript fragments for the exchange in progress, flushed to memory
    /// when the assistant stops speaking. Both halves are `Option` because an
    /// engine with transcription disabled sends neither.
    pending_turn: crate::memory::Turn,
    reconnect_attempts: u32,
    /// Capture resampler (mic rate -> engine's `input_format`), kept alive
    /// across frames with the rate pair that built it. It MUST NOT be rebuilt
    /// per frame: the filter carries state between calls, and a fresh one
    /// both loses that state and swallows any frame shorter than its chunk
    /// size. Keyed by rates because an engine switch (S6) can change the
    /// target rate mid-session.
    capture_resampler: Option<(u32, u32, Resampler)>,
    /// Playback resampler (engine's `output_format` -> speaker rate), same
    /// keep-alive rule as `capture_resampler`. A separate slot because the
    /// two directions almost never share a rate pair — e.g. OpenAI emits
    /// 24 kHz while a typical Windows default output device runs at 48 kHz —
    /// and writing engine audio to the sink unresampled plays it back at the
    /// wrong speed/pitch instead of silently doing nothing, so this is not
    /// optional the way a same-rate capture path can be.
    playback_resampler: Option<(u32, u32, Resampler)>,
    /// Every state transition, in order. Broadcast rather than `watch` because
    /// order is the payload: `Connecting` is what tells a user the app heard
    /// them, and it lives for microseconds — a latest-value channel would
    /// coalesce it away and the HUD would jump straight to `Listening`.
    state_tx: broadcast::Sender<State>,
    /// Latest *playback* level, 0.0..=1.0 — the assistant's voice as the sink
    /// is playing it, never the microphone. `watch` rather than broadcast for
    /// the opposite reason: at ~30 Hz nobody wants a backlog, only the newest
    /// value.
    level_tx: watch::Sender<f32>,
    /// Every engine error's message, in order — broadcast rather than `watch`
    /// for the same reason as `state_tx`: a transient error the shell missed
    /// because it coalesced away is a debugging dead end (this is precisely
    /// what let a bad `session.update` field go unnoticed — see the
    /// `uia-openai` voice/schema fix). No subscriber is normal (headless
    /// tests, a shell that doesn't care) and not an error.
    error_tx: broadcast::Sender<String>,
    /// Completed exchanges, as they are recorded.
    ///
    /// A broadcast rather than a watch, unlike the level: every turn matters
    /// and a shell that falls behind wants the ones it missed, not just the
    /// newest. Fired from `flush_turn` alongside the memory write, so a
    /// subscriber sees exactly what was persisted -- including the one-sided
    /// exchanges, which are real and should not be hidden from a live view
    /// when the stored history shows them.
    turn_tx: broadcast::Sender<crate::memory::Turn>,
    /// What the audio devices are waiting for, as the HUD should say it, or
    /// `None` while both are open. A `watch`: it is a state, and only the
    /// current one matters.
    audio_waiting: watch::Sender<Option<String>>,
    /// Why the last audio swap's opener failed, while the session runs on
    /// the silent detached pair it left. Held, not only sent on `error_tx`:
    /// a detached pair waits for nothing, so without this the waiting line
    /// would read `None` and a HUD that clears its error on reconnect would
    /// show a healthy session that hears and says nothing.
    audio_failed: Option<String>,
    /// Set when a terminal error is seen, so the run loop can stop instead of
    /// entering the backoff loop.
    terminal: bool,
    backoff: Backoff,
    /// Sent to the engine at setup. Owned here rather than in `SessionDeps`
    /// because it is a tuning knob the shell may change between connects, like
    /// the deadlines above.
    system_prompt: Option<String>,
    /// Sent to the engine at setup, same lifecycle as `system_prompt`.
    /// Engine-specific (OpenAI voice names vs. Nova voice IDs) — the shell
    /// picks a value that matches whichever engine it wired up.  Nova's
    /// `prompt_start_event` does not read this field at all; it hardcodes its
    /// own `voiceId` (see `uia-nova/src/protocol.rs`), so setting this has
    /// no effect there.
    voice: Option<String>,
    /// Ask the engine to transcribe the *user's* speech, sent at setup with
    /// the same lifecycle as `voice`.
    ///
    /// Only the user's half is in question here. The assistant's half is not
    /// gated by anything: `ModelTranscript` arrives from the engine's own
    /// output-audio transcript as a normal part of speaking. So a session
    /// with this off still records what the assistant said and never what
    /// the user said, which is why it is worth setting rather than leaving
    /// to the default.
    ///
    /// Costs latency and money, so the shell sets it from whether a memory
    /// backend is actually going to consume it.
    transcription: bool,
    /// The language hint for `transcription`; `None` lets the engine guess.
    transcription_language: Option<String>,
    /// The shell's handle onto this running session (`control.rs`). Owned
    /// here rather than in `SessionDeps` because it is not a port the session
    /// talks *through* — it is a switch the outside world flips while the run
    /// loop reads it, and a session nobody has attached a handle to keeps its
    /// own default (capture on) and behaves exactly as it did before handles
    /// existed.
    control: SessionControl,
}

/// Outcome of draining one buffered engine event.
enum Pump {
    /// An event was consumed; there may be more.
    Progressed,
    /// Nothing is queued right now, but the stream is still open.
    Drained,
    /// The engine closed, or was never connected.
    Closed,
}

impl Session {
    pub fn new(mut deps: SessionDeps) -> Self {
        let control = SessionControl::new();
        let text_rx = control.take_text_rx();
        let placeholder = Box::new(DetachedSource::new(deps.source.format()));
        let capture = audio::Capture::new(std::mem::replace(&mut deps.source, placeholder));
        let session = Self {
            deps,
            capture,
            state: State::Idle,
            events: None,
            stashed_event: None,
            text_rx,
            barge_in_threshold: DEFAULT_BARGE_IN_THRESHOLD,
            tool_deadline: Self::default_tool_deadline(),
            recall_deadline: Self::default_recall_deadline(),
            tools_deadline: Self::default_tools_deadline(),
            idle_deadline: Self::default_idle_deadline(),
            last_activity: None,
            connected_at: None,
            max_session_duration: None,
            pending_turn: crate::memory::Turn {
                user: None,
                assistant: None,
            },
            reconnect_attempts: 0,
            capture_resampler: None,
            playback_resampler: None,
            state_tx: broadcast::channel(STATE_BROADCAST_CAPACITY).0,
            level_tx: watch::channel(0.0).0,
            error_tx: broadcast::channel(ERROR_BROADCAST_CAPACITY).0,
            turn_tx: broadcast::channel(TURN_BROADCAST_CAPACITY).0,
            audio_waiting: watch::channel(None).0,
            audio_failed: None,
            terminal: false,
            backoff: Backoff::new(),
            system_prompt: None,
            voice: None,
            transcription: false,
            transcription_language: None,
            control,
        };
        session.publish_text_turn_support();
        session
    }

    /// Observe every state transition, in order. Subscribe before `run`.
    pub fn subscribe_state(&self) -> broadcast::Receiver<State> {
        self.state_tx.subscribe()
    }

    /// Observe the latest playback level for the HUD wave. Always readable; starts at 0.0.
    pub fn subscribe_level(&self) -> watch::Receiver<f32> {
        self.level_tx.subscribe()
    }

    /// Observe every engine error's message, in order. Subscribe before `run`.
    pub fn subscribe_error(&self) -> broadcast::Receiver<String> {
        self.error_tx.subscribe()
    }

    /// Observe every completed exchange as it is recorded. Subscribe before
    /// `run`.
    ///
    /// This is the live half of what `FileMemory` writes to disk. A shell can
    /// read the store back at any time; this is how it learns about a turn
    /// without polling for it.
    pub fn subscribe_turn(&self) -> broadcast::Receiver<crate::memory::Turn> {
        self.turn_tx.subscribe()
    }

    /// Observe what the audio devices are waiting for, e.g. `Waiting for the
    /// microphone "Jabra"`, or `None` while both are open. Always readable.
    ///
    /// After a failed audio swap this carries the failure instead (`audio
    /// devices: ...`) until a later swap opens its devices.
    pub fn subscribe_audio_waiting(&self) -> watch::Receiver<Option<String>> {
        self.audio_waiting.subscribe()
    }

    /// Republish the devices' waits, waking subscribers only on a change.
    fn publish_audio_waiting(&self) {
        let input = self.capture.waiting().and_then(|cell| cell.get());
        let output = self.deps.sink.waiting().and_then(|cell| cell.get());
        let message = self
            .audio_failed
            .clone()
            .or_else(|| audio::waiting_message(input.as_ref(), output.as_ref()));
        self.audio_waiting.send_if_modified(|current| {
            if *current == message {
                false
            } else {
                *current = message;
                true
            }
        });
    }

    pub fn default_tool_deadline() -> std::time::Duration {
        DEFAULT_TOOL_DEADLINE
    }

    pub fn default_recall_deadline() -> std::time::Duration {
        DEFAULT_RECALL_DEADLINE
    }

    pub fn default_tools_deadline() -> std::time::Duration {
        DEFAULT_TOOLS_DEADLINE
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn reconnect_attempts(&self) -> u32 {
        self.reconnect_attempts
    }

    pub fn set_barge_in_threshold(&mut self, t: f32) {
        self.barge_in_threshold = t;
    }

    pub fn set_tool_deadline(&mut self, d: std::time::Duration) {
        self.tool_deadline = d;
    }

    pub fn set_recall_deadline(&mut self, d: std::time::Duration) {
        self.recall_deadline = d;
    }

    pub fn set_tools_deadline(&mut self, d: std::time::Duration) {
        self.tools_deadline = d;
    }

    /// Zero disables the idle disconnect entirely, for a shell that would
    /// rather hold the connection open and take its chances.
    pub fn set_idle_deadline(&mut self, d: std::time::Duration) {
        self.idle_deadline = d;
    }

    pub fn default_idle_deadline() -> std::time::Duration {
        DEFAULT_IDLE_DEADLINE
    }

    /// The provider's own ceiling on how long one realtime session may last,
    /// regardless of activity. `None` means no cap is known and none is
    /// enforced. The shell supplies it per engine, since the three differ by
    /// almost an order of magnitude.
    pub fn set_max_session_duration(&mut self, d: Option<std::time::Duration>) {
        self.max_session_duration = d;
    }

    /// Is this connection close enough to the provider's ceiling to be worth
    /// replacing? Deliberately early -- see `SESSION_ROTATION_MARGIN`.
    fn session_too_old(&self) -> bool {
        let (Some(cap), Some(since)) = (self.max_session_duration, self.connected_at) else {
            return false;
        };
        since.elapsed() + SESSION_ROTATION_MARGIN >= cap
    }

    /// Adopt a pending prompt swap and rotate onto it.
    ///
    /// This is the same three-step rotation `session_too_old()` drives --
    /// flush, disconnect, connect -- for a second reason. Nothing new is
    /// invented: `connect()` already replays the conversation from memory,
    /// which is what makes a mid-dialogue identity change survivable.
    ///
    /// Extracted from the run loop rather than inlined so the ordering that
    /// matters (prompt set *before* connect, applied announced *after* it)
    /// is testable without driving a whole session.
    ///
    /// Success is read off `self.events`, not `self.terminal`. `disconnect()`
    /// clears the receiver, and `connect()` re-populates it only on the far
    /// side of a successful `engine.connect()` -- so `events.is_none()` here
    /// means precisely "no connection was established", covering the
    /// transient failure (which leaves `terminal` false) as well as the
    /// terminal one. Gating on `terminal` would announce a swap over a
    /// connection that never happened.
    async fn apply_pending_prompt_swap(&mut self) -> bool {
        let Some(swap) = self.control.take_prompt_swap() else {
            return false;
        };
        let previous = self.system_prompt.clone();

        self.flush_turn().await;
        self.disconnect().await;
        self.set_system_prompt(swap.prompt);
        self.connect().await;

        if self.events.is_none() {
            // Put the user back where they were. The same rotation path, run
            // once more with the old value -- a persona switch must not be
            // able to strand a session on an identity that would not connect.
            self.system_prompt = previous;
            // And say so. The shell answered the model optimistically, before
            // the loop had a chance to try, so silence here would leave it
            // believing an identity is live that this session never adopted.
            // Reported, not re-queued: a swap that retried itself would
            // rotate the session in a loop on a persistent failure.
            self.control.note_prompt_swap_failed(swap.tag);
            return true;
        }
        // Only now: this is how the shell learns the swap landed, so
        // announcing it before a successful connect would report an identity
        // the engine was never given.
        self.control.note_prompt_swap_applied(swap.tag);
        self.mark_activity();
        true
    }

    /// Adopt a pending audio swap. Returns whether there was one.
    ///
    /// The threshold always applies. With an opener, the old pair is dropped
    /// *before* the new one opens (VPIO allows one unit per process), both on
    /// a blocking thread because closing and opening a real device blocks.
    /// The engine connection is untouched: a connected session keeps talking
    /// and only its audio moves.
    ///
    /// An opener that fails or panics leaves the session on a detached pair
    /// -- silent, but alive and still able to take the next swap -- and says
    /// why on `error_tx`. Stranding the session would leave the user no way
    /// back short of a restart. The failure is also held on the waiting line
    /// and on the handle (`audio_swap_failed`) until a swap opens its
    /// devices, so the HUD keeps saying it and an unchanged Save retries.
    async fn apply_pending_audio_swap(&mut self) -> bool {
        let Some(swap) = self.control.take_audio_swap() else {
            return false;
        };
        self.barge_in_threshold = swap
            .barge_in_threshold
            .unwrap_or(DEFAULT_BARGE_IN_THRESHOLD);
        let Some(open) = swap.open else {
            return true;
        };

        let source = self.capture.stop().await;
        let source_format = source.format();
        let sink_format = self.deps.sink.format();
        let sink = std::mem::replace(
            &mut self.deps.sink,
            Box::new(DetachedSink::new(sink_format)),
        );
        let opened = tokio::task::spawn_blocking(move || {
            drop(source);
            drop(sink);
            open()
        })
        .await;

        let (source, sink): AudioPair = match opened {
            Ok(Ok(pair)) => {
                self.audio_failed = None;
                pair
            }
            Ok(Err(e)) => self.detached_pair(source_format, sink_format, &e),
            Err(e) => self.detached_pair(source_format, sink_format, &e),
        };
        self.control
            .note_audio_swap_failed(self.audio_failed.is_some());
        self.capture = audio::Capture::new(source);
        self.deps.sink = sink;
        // Both resamplers were keyed to the old devices' rates and carry
        // their filter state; the new devices start clean.
        self.capture_resampler = None;
        self.playback_resampler = None;
        let _ = self.level_tx.send(0.0);
        self.publish_audio_waiting();
        true
    }

    /// The pair a failed swap leaves behind, after reporting why and holding
    /// the failure for the waiting line.
    fn detached_pair(
        &mut self,
        source_format: crate::audio::AudioFormat,
        sink_format: crate::audio::AudioFormat,
        cause: &dyn std::fmt::Display,
    ) -> AudioPair {
        let message = format!("audio devices: {cause}");
        let _ = self.error_tx.send(message.clone());
        self.audio_failed = Some(message);
        (
            Box::new(DetachedSource::new(source_format)),
            Box::new(DetachedSink::new(sink_format)),
        )
    }

    /// Something was said. Resets the idle countdown.
    fn mark_activity(&mut self) {
        self.last_activity = Some(std::time::Instant::now());
    }

    /// Has the session been silent long enough to drop the connection?
    /// False while disconnected (`last_activity` is `None`) and false when the
    /// deadline is zero, which disables the behaviour.
    fn idle_expired(&self) -> bool {
        if self.idle_deadline.is_zero() {
            return false;
        }
        self.last_activity
            .is_some_and(|t| t.elapsed() >= self.idle_deadline)
    }

    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = Some(prompt.into());
    }

    pub fn set_voice(&mut self, voice: impl Into<String>) {
        self.voice = Some(voice.into());
    }

    /// Ask the engine for user-speech transcripts. See the field's own note:
    /// without this the stored conversation is one-sided, holding every
    /// assistant reply and none of the user's own words.
    pub fn set_transcription(&mut self, on: bool) {
        self.transcription = on;
    }

    /// The language to transcribe the user in, as an ISO-639-1 code. `None`
    /// lets the engine guess.
    pub fn set_transcription_language(&mut self, language: Option<String>) {
        self.transcription_language = language;
    }

    /// Adopt the shell's handle, replacing the default one. Called before
    /// `run`, with a clone of the same handle the shell keeps: the shell has
    /// to be able to `.manage()` the handle at startup, long before
    /// `build_session` has produced a `Session` to ask for one.
    pub fn set_control(&mut self, control: SessionControl) {
        // Adopt whichever text queue comes with the new handle — the shell's
        // clone may have a receiver still sitting untaken (the ordinary
        // case) or none (a stale/already-adopted handle), in which case this
        // session keeps whatever it already had.
        if let Some(rx) = control.take_text_rx() {
            self.text_rx = Some(rx);
        }
        self.control = control;
        // The adopted handle starts on its own `Unknown` default, so the
        // engine's answer has to be published into it too — otherwise the
        // shell reads `Unknown` for a Bedrock session that has always been
        // `Unsupported`, which is the exact silent-lie this plan removes.
        self.publish_text_turn_support();
    }

    /// A clone of this session's handle. Every clone is the same switch, so
    /// this is equally a way to drive a session from a test.
    pub fn control(&self) -> SessionControl {
        self.control.clone()
    }

    /// This session's typed-turn capability (PLAN.md FD2) — the engine's own
    /// answer, or `Unsupported` if FD3's degrade has since fired.
    pub fn text_turn_support(&self) -> TextTurnSupport {
        self.control.text_turn_support()
    }

    /// Re-read the capability from the engine and publish it on the handle.
    /// Called where the *engine* changes — construction, handle adoption, an
    /// engine switch — and deliberately NOT on connect: a reconnect to the
    /// same engine must not undo a degrade that engine has already earned.
    fn publish_text_turn_support(&self) {
        self.control
            .publish_text_turn_support(self.deps.engine.text_turn_support());
    }

    /// Engine selection may only change while fully idle. Rejecting the switch
    /// here removes every mid-flight teardown case: no draining in-flight
    /// audio, no cancelling a pending tool call, no closing a stream
    /// mid-utterance.
    pub fn switch_engine(&mut self, engine: Box<dyn S2sEngine>) -> Result<(), SwitchError> {
        if !can_switch_engine(self.state) {
            return Err(SwitchError::NotIdle(self.state));
        }
        self.deps.engine = engine;
        self.events = None;
        // A new engine is a new answer, including an optimistic one: this is
        // the one transition allowed to lift a degrade, because the engine
        // that earned it is no longer the one running.
        self.publish_text_turn_support();
        Ok(())
    }

    fn apply(&mut self, input: SessionInput) {
        let Some(next) = transition(self.state, &input) else {
            // invalid for this state: log and drop
            return;
        };
        if next == self.state {
            return;
        }
        self.state = next;
        // No receivers is normal (headless tests, the contract suite); it is
        // not an error worth surfacing.
        let _ = self.state_tx.send(next);
    }

    /// Drop the engine connection and return to `Idle`, keeping the session
    /// alive and reusable. The inverse of [`Session::connect`].
    ///
    /// Nothing here is new machinery: `S2sEngine::close` has always been on
    /// the trait and implemented by every engine, and `transition` has always
    /// unwound any active state to `Idle` on `Deactivate`. Until now nothing
    /// called either.
    ///
    /// Idempotent on purpose. The shell asking for this is a UI gesture -- a
    /// user opening Settings twice, an idle timer firing while a disconnect is
    /// already in flight -- and the second ask must be a no-op, not an error.
    ///
    /// A failing `close()` is reported and then ignored: a connection we have
    /// decided to abandon is abandoned whether or not the far end agreed. The
    /// alternative -- staying `Listening` because the socket would not shut
    /// cleanly -- is the failure mode this exists to prevent.
    pub async fn disconnect(&mut self) {
        if self.state == State::Idle && self.events.is_none() {
            return;
        }
        if let Err(e) = self.deps.engine.close().await {
            let _ = self.error_tx.send(format!("closing the engine: {e}"));
        }
        self.events = None;
        self.connected_at = None;
        // Collapse the meter on the way out. The rotation path disconnects
        // and reconnects without returning to the pump in between, so nothing
        // else would publish a level for the whole gap and the HUD would hold
        // the last one it was given -- a ribbon frozen mid-shape over a
        // session that is no longer listening to anything. Same rule the
        // capture gate in `handle_frame` already states: a level that keeps
        // showing while the audio goes nowhere reads as "you are being
        // heard".
        let _ = self.level_tx.send(0.0);
        self.apply(SessionInput::Deactivate);
    }

    async fn connect(&mut self) {
        self.apply(SessionInput::Activate);
        self.connected_at = Some(std::time::Instant::now());

        // Recall sits on the critical path to first audio, so it gets a hard
        // deadline. Memory is an enhancement and must never gate connecting.
        let memory_context = match tokio::time::timeout(
            self.recall_deadline,
            self.deps.memory.recall(&self.deps.ctx, None),
        )
        .await
        {
            Ok(Ok(items)) => items,
            Ok(Err(_)) | Err(_) => Vec::new(),
        };

        // Ask the executor what it can do, every connect. Doing it here rather
        // than once at construction means a reconnect after an MCP server comes
        // back picks its tools up, and a server that is down costs the session
        // its tools rather than the ability to talk at all.
        //
        // Time-boxed for the same reason recall above it is, and it is the same
        // sentence that applies: this sits on the critical path to first audio.
        // A server that is *down* answers promptly with an error and needs no
        // deadline; one that is wedged never answers at all, and without this
        // it holds `connect()` open forever. That matters most on the rotation
        // path, where a persona switch runs the whole of `connect()` with the
        // user waiting on it. `McpRouter` time-boxes each server underneath
        // this, so reaching this outer bound means something below it did not
        // honour its own -- hence the deliberately longer ceiling.
        let tools = match tokio::time::timeout(self.tools_deadline, self.deps.executor.list_tools())
            .await
        {
            Ok(Ok(tools)) => tools,
            Ok(Err(_)) | Err(_) => Vec::new(),
        };

        let (tx, rx) = mpsc::channel(64);
        let cfg = SessionConfig {
            memory_context,
            tools,
            system_prompt: self.system_prompt.clone(),
            voice: self.voice.clone(),
            transcription: self.transcription,
            transcription_language: self.transcription_language.clone(),
        };
        if let Err(e) = self.deps.engine.connect(&cfg, tx).await {
            // Same reasoning as `EngineEvent::Error` below: a `connect()`
            // failure (e.g. an invalid API key rejected before any event
            // stream exists) is the only chance this message ever gets
            // broadcast. Without this, an invalid key surfaced nothing but
            // the bare `Terminal` outcome at the end of `run()` — no reason,
            // just the fact that something stopped it.
            let _ = self.error_tx.send(e.to_string());
            if e.is_terminal() {
                self.terminal = true;
                self.apply(SessionInput::TerminalError);
            } else {
                self.reconnect_attempts += 1;
                self.apply(SessionInput::TransientError);
            }
            return;
        }
        self.events = Some(rx);
    }

    /// Resample `frame` from `from` to `to` using `slot` as the kept-alive
    /// filter state (see `capture_resampler`/`playback_resampler`). `None`
    /// means the frame must be dropped, not sent at the wrong rate — either
    /// the rate pair was rejected (e.g. a zero rate) or the resampler is
    /// still filling its first chunk. An associated function, not a method,
    /// so it can be called with a disjoint borrow of one resampler slot while
    /// the rest of `self` (e.g. `self.deps`) stays free for the caller to use
    /// in the same statement.
    fn resample(
        slot: &mut Option<(u32, u32, Resampler)>,
        frame: Vec<i16>,
        from: u32,
        to: u32,
    ) -> Option<Vec<i16>> {
        if from == to {
            return Some(frame);
        }
        let stale = !matches!(slot, Some((f, t, _)) if *f == from && *t == to);
        if stale {
            *slot = Some((from, to, Resampler::new(from, to, 1024).ok()?));
        }
        let (_, _, r) = slot.as_mut().expect("just populated");
        let out = r.process(&frame).ok()?;
        if out.is_empty() { None } else { Some(out) }
    }

    /// Engines stream transcripts in fragments, so each arrives as another
    /// piece of the same utterance rather than a replacement for it.
    fn append_fragment(slot: &mut Option<String>, text: &str) {
        if text.is_empty() {
            return;
        }
        match slot {
            Some(existing) => {
                if !existing.ends_with(' ') && !text.starts_with(' ') {
                    existing.push(' ');
                }
                existing.push_str(text);
            }
            None => *slot = Some(text.to_string()),
        }
    }

    /// Write the completed exchange to memory and start a fresh one.
    ///
    /// A failing backend is reported and dropped: memory is an enhancement
    /// (`connect` already treats recall that way, with a hard deadline), and a
    /// session that stopped talking because its notes could not be filed would
    /// be a worse outcome than forgetting.
    async fn flush_turn(&mut self) {
        if self.pending_turn.user.is_none() && self.pending_turn.assistant.is_none() {
            return;
        }
        let turn = std::mem::replace(
            &mut self.pending_turn,
            crate::memory::Turn {
                user: None,
                assistant: None,
            },
        );
        if let Err(e) = self.deps.memory.record(&self.deps.ctx, &turn).await {
            let _ = self.error_tx.send(format!("recording the turn: {e}"));
        }
        // After the write, not before: a subscriber being told about a turn
        // that then failed to persist would show a live line the history card
        // will not have when it is next opened. The send is still
        // unconditional, because a failed write is a storage problem and the
        // exchange did happen -- the error above is what reports the failure.
        let _ = self.turn_tx.send(turn);
    }

    /// Apply one engine event. Returns false when the stream should stop.
    async fn handle_event(&mut self, ev: EngineEvent) -> bool {
        match ev {
            EngineEvent::Ready => self.apply(SessionInput::EngineReady),
            EngineEvent::SpeechStarted => self.apply(SessionInput::SpeechStarted),
            EngineEvent::SpeechEnded => self.apply(SessionInput::SpeechEnded),
            // Writing the exchange down is deliberately NOT done above. The
            // assistant having stopped talking does not mean its transcript
            // has arrived: OpenAI's audio-done can precede its
            // transcript-done, so flushing at `SpeechEnded` recorded whichever
            // halves had landed and pushed the rest into the next exchange.
            // Every entry in a store written that way is one-sided and
            // attributed to the wrong turn.
            //
            // `TurnComplete` is the engine saying every transcript belonging
            // to this exchange has been sent. It carries no state transition:
            // the session is already Listening by the time it arrives, and
            // making it wait for this would delay the HUD behind a bookkeeping
            // event.
            EngineEvent::TurnComplete => self.flush_turn().await,
            // Both were previously swallowed by the catch-all arm below: the
            // engines have always sent them and nothing ever read them, which
            // is why a reconnect started from a blank slate. Appended rather
            // than assigned because engines emit transcripts in fragments.
            EngineEvent::UserTranscript(text) => {
                Self::append_fragment(&mut self.pending_turn.user, &text);
            }
            EngineEvent::ModelTranscript(text) => {
                Self::append_fragment(&mut self.pending_turn.assistant, &text);
            }
            EngineEvent::AudioChunk(chunk) => {
                // The engine speaks at its own `output_format` rate (24 kHz
                // for OpenAI); the sink plays back at whatever rate the OS
                // default output device negotiated (commonly 48 kHz on
                // Windows). Writing `chunk` straight through without this
                // plays it back at the wrong speed and pitch — the "bad
                // mechanical synthesized voice" a rate mismatch produces.
                let from = self.deps.engine.output_format().sample_rate_hz;
                let to = self.deps.sink.format().sample_rate_hz;
                if let Some(chunk) = Self::resample(&mut self.playback_resampler, chunk, from, to) {
                    let _ = self.deps.sink.write(&chunk).await;
                }
            }
            EngineEvent::Interrupted => self.apply(SessionInput::Interrupted),
            EngineEvent::ToolCall { id, name, args } => {
                self.apply(SessionInput::ToolCalled);
                let result = match self
                    .deps
                    .executor
                    .execute(&name, args, self.tool_deadline)
                    .await
                {
                    Ok(r) => r,
                    // A failing tool is data for the model, never a dead
                    // session: the assistant speaks the failure.
                    Err(e) => crate::tools::ToolResult::error(e.to_string()),
                };
                let _ = self.deps.engine.send_tool_result(id, result).await;
                self.apply(SessionInput::ToolFinished);
            }
            EngineEvent::Error(e) => {
                // No receivers is normal (headless tests, the contract
                // suite); it is not an error worth surfacing, same as
                // `state_tx`'s send above.
                let _ = self.error_tx.send(e.to_string());
                if e.is_terminal() {
                    self.terminal = true;
                    self.apply(SessionInput::TerminalError);
                } else {
                    self.reconnect_attempts += 1;
                    self.apply(SessionInput::TransientError);
                }
            }
            EngineEvent::Closed => return false,
            // No catch-all arm, deliberately. `UserTranscript` and
            // `ModelTranscript` were swallowed by one for the life of this
            // project: the engines sent them, nothing read them, and every
            // reconnect therefore started from a blank slate. An exhaustive
            // match means the next event added has to be handled or explicitly
            // ignored, rather than quietly disappearing.
        }
        true
    }

    /// Pump one engine event, awaiting one if none is queued. Returns false
    /// when the stream has closed.
    ///
    /// `pub` because the real run loop is composed in `uia-app` (Task 28) —
    /// a different crate — so this and `pump_audio_once` are the two halves
    /// it drives. Nothing inside `uia-core` calls it yet; the deterministic
    /// test drivers below use the non-blocking variant instead.
    pub async fn pump_engine(&mut self) -> bool {
        let Some(rx) = self.events.as_mut() else {
            return false;
        };
        let Some(ev) = rx.recv().await else {
            return false;
        };
        self.handle_event(ev).await
    }

    /// Alias for `pump_engine`, named to match the test drivers' `_once`
    /// convention used elsewhere (`pump_audio_once`).
    pub async fn pump_engine_once(&mut self) -> bool {
        self.pump_engine().await
    }

    /// Pump one *already queued* engine event. Never awaits new traffic, so a
    /// caller can drain to quiescence without any risk of blocking.
    async fn pump_buffered_engine(&mut self) -> Pump {
        let Some(rx) = self.events.as_mut() else {
            return Pump::Closed;
        };
        let ev = match self.stashed_event.take() {
            Some(ev) => Ok(ev),
            None => rx.try_recv(),
        };
        let ev = match ev {
            Ok(ev) => ev,
            Err(mpsc::error::TryRecvError::Empty) => return Pump::Drained,
            Err(mpsc::error::TryRecvError::Disconnected) => return Pump::Closed,
        };
        if self.handle_event(ev).await {
            Pump::Progressed
        } else {
            Pump::Closed
        }
    }

    /// Drive one captured frame into the engine, resampling to its rate.
    /// Returns false when the source is exhausted.
    pub async fn pump_audio_once(&mut self) -> bool {
        let Some(frame) = self.capture.recv().await else {
            return false;
        };
        self.handle_frame(frame).await;
        true
    }

    /// Forward one captured frame: meter, capture gate, barge-in, resample,
    /// send.
    async fn handle_frame(&mut self, frame: audio::CapturedFrame) {
        let audio::CapturedFrame {
            samples: frame,
            sample_rate_hz: from,
        } = frame;

        // The meter is the assistant's voice, read from the sink once per
        // captured frame (~20 ms) while the microphone delivers, and on the
        // run loop's `POLL_INTERVAL` tick while it does not. Published before
        // the capture gate on purpose: muting the mic does not mute the
        // assistant, and a typed turn answered aloud while muted must still
        // move the wave.
        let _ = self.level_tx.send(self.deps.sink.level());

        // Gated capture (PLAN.md FD15). The frame is read from the device
        // either way — stopping the read would let the capture buffer back up
        // and hand the engine a burst of stale audio on unmute — and then
        // dropped here, before barge-in, before any resample or `send_audio`.
        if !self.control.capture_enabled() {
            return;
        }

        // Counted here, past the capture gate, on purpose: a muted mic is not
        // activity. That is what lets `mute and walk away` reach the idle
        // deadline and drop the connection, which is the case this exists for.
        self.mark_activity();

        // The user's level feeds barge-in only. It used to be published as the
        // meter too, which made the wave follow the user's voice rather than
        // the assistant's.
        let level = crate::audio::rms(&frame);

        // Barge-in: clear the local buffer FIRST and synchronously. Waiting
        // for the provider to acknowledge would let the assistant keep
        // talking for a full round trip, which is what makes an assistant
        // feel broken.
        if self.state == State::Speaking && level > self.barge_in_threshold {
            self.deps.sink.clear();
            self.apply(SessionInput::UserSpoke);
            let _ = self.deps.engine.interrupt().await;
        }

        // `from` is the rate this frame was captured at, not what the source
        // reports now: capture reads ahead, and a device that reopened at a
        // new rate in between would otherwise be resampled from the old one.
        let to = self.deps.engine.input_format().sample_rate_hz;
        // The resampler buffers until it has a full chunk, so early frames
        // legitimately produce nothing yet — that is not an error, just not
        // this call's turn to send.
        if let Some(frame) = Self::resample(&mut self.capture_resampler, frame, from, to) {
            let _ = self.deps.engine.send_audio(&frame).await;
        }
    }

    /// Drain one already-queued typed turn (PLAN.md S4), if any. Never awaits
    /// new input — same non-blocking contract as `pump_buffered_engine`, so
    /// callers can drain to quiescence without risking a stall on the
    /// capture/engine pump cycle. A `send_text` failure (an engine with no
    /// text-input path, or a transport error) is reported on `error_tx`
    /// exactly like an `EngineEvent::Error`, but deliberately does NOT flip
    /// `terminal` or bump `reconnect_attempts`: a typed turn a connected
    /// engine can't accept is not a connection problem, and treating it as
    /// one would reconnect a perfectly healthy session. It does, however,
    /// degrade this session's `TextTurnSupport` to `Unsupported` (FD3) — one
    /// backend transition every UI surface follows, rather than each surface
    /// inventing its own reaction to the same error string.
    async fn pump_text(&mut self) -> Pump {
        let Some(rx) = self.text_rx.as_mut() else {
            return Pump::Drained;
        };
        match rx.try_recv() {
            Ok(text) => {
                self.send_text_turn(text).await;
                Pump::Progressed
            }
            Err(mpsc::error::TryRecvError::Empty) => Pump::Drained,
            Err(mpsc::error::TryRecvError::Disconnected) => Pump::Drained,
        }
    }

    /// Send one typed turn, whether drained by `pump_text` or received by the
    /// run loop's `select!`.
    async fn send_text_turn(&mut self, text: String) {
        if let Err(e) = self.deps.engine.send_text(&text).await {
            let _ = self.error_tx.send(e.to_string());
            // FD3's degrade, in the ONE place every engine's typed turns pass
            // through. A turn the engine actually refused is the
            // authoritative answer to "can this session take typed text?",
            // and it outranks whatever the engine claimed — including an
            // optimistic `Unknown`.
            self.control.degrade_text_turn_support(e.to_string());
        }
    }

    /// `pub` for the same reason as `pump_engine_once`/`pump_audio_once`:
    /// typed turns don't take part in either alternation loop those drive,
    /// so a test exercises this one directly. Returns whether a turn was
    /// actually sent to the engine (success or failure both count — only an
    /// empty queue returns `false`).
    pub async fn pump_text_once(&mut self) -> bool {
        matches!(self.pump_text().await, Pump::Progressed)
    }

    /// Read the next activation request. Used by the real run loop (Task 28);
    /// the deterministic drivers below connect directly instead.
    pub async fn wait_for_activation(&mut self) -> Option<ActivationEvent> {
        self.deps.activation.next().await
    }

    /// The real run loop: wait to be activated, connect, then make one pass
    /// of checks after another, waiting between passes for whichever input
    /// arrives first, until something ends the session.
    ///
    /// **Why it `select!`s.** Each pass first runs, in a fixed order, every
    /// check whose ordering carries meaning: drain the engine, drain typed
    /// turns, reconnect, close, reset the backoff, park, swap the prompt,
    /// swap the audio, rotate, idle out. Then it waits on one biased
    /// `select!` over the next captured frame, the engine's events, typed
    /// turns, the connection-wanted switch and a `POLL_INTERVAL` tick,
    /// handles the one that came, and goes round again. Capture runs on its
    /// own task (`session/audio.rs`) precisely so it can be one arm among
    /// these: when the loop awaited the microphone directly, a device that
    /// yielded nothing froze engine playback, parking, rotation and the idle
    /// deadline with it.
    ///
    /// Capture is the first arm. Behind a fast-streaming reply the events arm
    /// was always ready, so barge-in frames piled up in capture's short queue
    /// and arrived late. Capture first cannot starve the rest: a device
    /// delivers about one frame per 20 ms, and the drain at the top of every
    /// pass handles queued engine events whichever arm woke the loop.
    ///
    /// An engine event is not handled in its arm: it goes into
    /// `stashed_event` for the next drain, so the reconnect and barge-in
    /// ordering below is the same whichever input woke the loop. A typed turn
    /// and a captured frame are handled in their arms, since neither changes
    /// what the checks in the pass decide. The tick exists for the passes
    /// nothing else would trigger -- a silent microphone with a quiet engine
    /// -- and publishes the playback level so the meter still follows the
    /// assistant's voice with no frames arriving to clock it.
    ///
    /// Reconnection is driven from `reconnect_attempts` (S6) rather than from
    /// the state, because `Connecting` is also the normal pre-`Ready` state and
    /// retrying on that would reconnect a session that is merely still starting.
    pub async fn run(&mut self) -> RunOutcome {
        // Nothing happens until the user asks for it. A dropped channel here
        // means the shell is quitting before the first activation.
        if self.deps.activation.next().await.is_none() {
            return RunOutcome::ShutDown;
        }

        let mut wanted = self.control.subscribe_connection_wanted();
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        'connected: loop {
            // Park until a connection is wanted. A closed channel means every
            // `SessionControl` clone is gone, i.e. the shell has quit.
            while !*wanted.borrow() {
                self.last_activity = None;
                if wanted.changed().await.is_err() {
                    return RunOutcome::ShutDown;
                }
            }

            // Settings closing is what woke the park, and Settings is what
            // asks for new devices, so they open here, before the reconnect.
            self.apply_pending_audio_swap().await;

            // Captured BEFORE the first connect: that connect can itself fail
            // transiently, and the loop below must see that as a retry to make.
            let mut attempts = self.reconnect_attempts;
            self.connect().await;
            if self.terminal {
                return RunOutcome::Terminal;
            }
            self.mark_activity();

            loop {
                self.publish_audio_waiting();
                let mut closed = false;
                loop {
                    match self.pump_buffered_engine().await {
                        // The assistant talking is activity too: a long answer, or
                        // a slow tool call, must not trip the idle deadline just
                        // because the user is listening rather than speaking.
                        Pump::Progressed => {
                            self.mark_activity();
                            continue;
                        }
                        Pump::Drained => break,
                        Pump::Closed => {
                            closed = true;
                            break;
                        }
                    }
                }
                if self.terminal {
                    return RunOutcome::Terminal;
                }

                // Typed turns (PLAN.md S4) drain the same way: to quiescence,
                // never awaiting new input, so a user who submits several in a
                // row before the engine replies doesn't stall the audio pump
                // below by so much as one frame.
                while let Pump::Progressed = self.pump_text().await {}

                // Checked before `closed`: a transient failure also leaves the
                // stream closed, and retrying it is the whole point.
                if self.reconnect_attempts > attempts {
                    attempts = self.reconnect_attempts;
                    let delay = self.backoff.next_delay();
                    tokio::time::sleep(delay).await;
                    self.events = None;
                    self.connect().await;
                    if self.terminal {
                        return RunOutcome::Terminal;
                    }
                    continue;
                }
                if closed {
                    return RunOutcome::EngineClosed;
                }

                // Reaching Listening is the only evidence the connection is
                // genuinely healthy, so it is what resets the backoff — not a
                // successful `connect()`, which an engine can return before the
                // session is usable.
                if self.state == State::Listening {
                    self.backoff.reset();
                }

                // Checked here, in its place in the pass, rather than in the
                // `select!` arm that wakes on the watch: the arm only wakes
                // the loop, so a request is acted on in the same order as
                // every other check, after the reconnect and close above.
                if !*wanted.borrow() {
                    self.disconnect().await;
                    continue 'connected;
                }

                // Before the rotation check, not after: a user who just
                // asked for a switch is waiting on it right now, while the
                // rotation deadline carries 30s of margin and can afford to
                // wait one more pump.
                if self.apply_pending_prompt_swap().await {
                    if self.terminal {
                        return RunOutcome::Terminal;
                    }
                    continue;
                }

                // Not expected from the UI, which swaps while parked, but a
                // swap can arrive while connected; it moves the audio without
                // touching the connection.
                if self.apply_pending_audio_swap().await {
                    continue;
                }

                // Nobody has said anything for `idle_deadline`. Drop the
                // connection before the provider drops it for us: a disconnect we
                // chose is a state the UI can show and the user can undo, while a
                // remote close arrives as an error in the middle of a session.
                //
                // `request_disconnect` as well as `disconnect`, or the park above
                // would see a connection still wanted and immediately reconnect --
                // and the shell would have no idea the connection had gone.
                // Rotate before the provider's ceiling rather than after.
                // Reconnecting immediately (rather than parking, as the idle
                // path does) is what makes this invisible: `connect()` recalls
                // from memory, so the new session resumes with the
                // conversation the old one had.
                if self.session_too_old() {
                    self.flush_turn().await;
                    self.disconnect().await;
                    self.connect().await;
                    if self.terminal {
                        return RunOutcome::Terminal;
                    }
                    self.mark_activity();
                    continue;
                }

                if self.idle_expired() {
                    self.control.request_disconnect();
                    // Mute too, returning the app to exactly its cold state.
                    // Otherwise the mic still reads as live against a session
                    // that is gone, and the user speaks into a disconnected app
                    // with no sign anything is wrong. Muting makes the next
                    // unmute the gesture that reconnects — the same gesture
                    // that started the first session.
                    self.control.set_capture_enabled(false);
                    self.flush_turn().await;
                    self.disconnect().await;
                    continue 'connected;
                }

                tokio::select! {
                    biased;
                    // First, so a barge-in frame is never queued behind a
                    // streaming reply. It cannot starve the arms below: the
                    // device paces it to one frame per ~20 ms.
                    frame = self.capture.recv() => match frame {
                        Some(frame) => self.handle_frame(frame).await,
                        None => return RunOutcome::SourceExhausted,
                    },
                    ev = recv_or_never(&mut self.events) => {
                        // `None` is the stream closing: the next drain sees
                        // `Closed` and handles it like any other close.
                        self.stashed_event = ev;
                    }
                    text = recv_or_never(&mut self.text_rx) => match text {
                        Some(text) => self.send_text_turn(text).await,
                        // Every sender is gone, so nothing can arrive on it
                        // again; dropping it stops this arm firing forever.
                        None => self.text_rx = None,
                    },
                    changed = wanted.changed() => {
                        if changed.is_err() {
                            return RunOutcome::ShutDown;
                        }
                    }
                    _ = tick.tick() => {
                        let _ = self.level_tx.send(self.deps.sink.level());
                    }
                }
            }
        }
    }

    // --- test drivers -----------------------------------------------------
    // Deterministic pumps so headless tests never rely on wall-clock timing.

    /// Connect, then drain every engine event already queued, asserting the
    /// session settles on `target`.
    ///
    /// Note this drains to *quiescence* rather than stopping the moment
    /// `target` is first seen. A scripted engine delivers a whole turn up
    /// front, and `Ready, SpeechStarted, AudioChunk, SpeechEnded` passes
    /// through `Listening` on its very first event — stopping there would skip
    /// the audio the caller is trying to observe.
    pub async fn run_until_idle_or(&mut self, target: State) {
        if self.state == State::Idle {
            self.connect().await;
        }
        for _ in 0..MAX_PUMPS {
            match self.pump_buffered_engine().await {
                Pump::Progressed => continue,
                Pump::Drained | Pump::Closed => break,
            }
        }
        debug_assert!(
            self.state == target || self.state == State::Idle,
            "drained the engine but settled in {:?}, not {target:?}",
            self.state
        );
    }

    pub async fn run_until_source_exhausted(&mut self) {
        if self.state == State::Idle {
            self.connect().await;
        }
        // Drain the Ready event so we are Listening before streaming audio.
        let _ = self.pump_buffered_engine().await;
        while self.pump_audio_once().await {}
    }
}

/// `rx.recv()`, or never for a channel that is not there: a disconnected
/// session has no engine stream, and its `select!` arm must not fire.
async fn recv_or_never<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use crate::audio::{AudioFormat, FixtureSource, VecSink};
    use crate::engine::FakeEngine;
    use crate::memory::NullMemory;
    use crate::tools::FakeExecutor;
    use std::time::Duration;

    fn a_session() -> Session {
        let (activation, _tx) = crate::activation::ChannelActivation::new();
        Session::new(SessionDeps {
            source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
            sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
            engine: Box::new(FakeEngine::new(vec![])),
            executor: std::sync::Arc::new(FakeExecutor::new()),
            memory: std::sync::Arc::new(NullMemory),
            activation: Box::new(activation),
            ctx: MemoryContext {
                user_id: "u".into(),
                conversation_id: "c".into(),
            },
        })
    }

    /// A parked session must never be judged idle. `last_activity` is `None`
    /// while disconnected, and without this guard the run loop would decide a
    /// session it just disconnected was overdue for disconnecting.
    #[test]
    fn a_session_that_has_said_nothing_yet_is_not_idle() {
        let session = a_session();
        assert!(session.last_activity.is_none());
        assert!(!session.idle_expired());
    }

    /// The deadline actually fires. Uses a zero-length window rather than
    /// sleeping, so this stays a logic test and not a timing one.
    #[test]
    fn activity_older_than_the_deadline_is_idle() {
        let mut session = a_session();
        session.set_idle_deadline(Duration::from_nanos(1));
        session.mark_activity();
        std::thread::sleep(Duration::from_millis(2));
        assert!(session.idle_expired());
    }

    /// Fresh activity resets the countdown -- the property that keeps a long
    /// answer or a slow tool call from tripping the deadline.
    #[test]
    fn fresh_activity_is_not_idle() {
        let mut session = a_session();
        session.set_idle_deadline(Duration::from_secs(60));
        session.mark_activity();
        assert!(!session.idle_expired());
    }

    /// Zero means disabled, not "expire immediately". A shell that opts out
    /// must keep its connection however long it sits quiet.
    #[test]
    fn a_zero_deadline_never_expires() {
        let mut session = a_session();
        session.set_idle_deadline(Duration::ZERO);
        session.mark_activity();
        std::thread::sleep(Duration::from_millis(2));
        assert!(!session.idle_expired());
    }

    /// A microphone that reopens at a new rate between two frames, as a
    /// device switch does: 24 kHz for the first frame, 48 kHz after.
    struct ReopenedAtANewRate {
        yielded: usize,
    }

    #[async_trait::async_trait]
    impl AudioSource for ReopenedAtANewRate {
        fn format(&self) -> AudioFormat {
            let hz = if self.yielded <= 1 { 24_000 } else { 48_000 };
            AudioFormat::mono_pcm16(hz)
        }
        async fn next_frame(&mut self) -> Option<Vec<i16>> {
            self.yielded += 1;
            Some(vec![0; 480])
        }
    }

    /// Capture reads ahead of the session, so the rate the source reports
    /// when a frame is handled can already belong to a later frame. The rate
    /// has to travel with each frame, or a reopened device's audio is
    /// resampled from the old rate and reaches the engine at the wrong pitch.
    #[tokio::test]
    async fn the_capture_rate_is_read_per_frame_so_a_reopened_device_at_a_new_rate_is_resampled() {
        let mut session = a_session();
        session.capture = audio::Capture::new(Box::new(ReopenedAtANewRate { yielded: 0 }));

        assert!(session.pump_audio_once().await);
        assert!(session.pump_audio_once().await);

        let (from, to, _) = session
            .capture_resampler
            .as_ref()
            .expect("both frames needed resampling");
        assert_eq!((*from, *to), (48_000, 16_000));
    }
}

#[cfg(test)]
mod session_cap_tests {
    use super::*;
    use crate::audio::{AudioFormat, FixtureSource, VecSink};
    use crate::engine::FakeEngine;
    use crate::memory::NullMemory;
    use crate::tools::FakeExecutor;
    use std::time::Duration;

    fn a_session() -> Session {
        let (activation, _tx) = crate::activation::ChannelActivation::new();
        Session::new(SessionDeps {
            source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
            sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
            engine: Box::new(FakeEngine::new(vec![])),
            executor: std::sync::Arc::new(FakeExecutor::new()),
            memory: std::sync::Arc::new(NullMemory),
            activation: Box::new(activation),
            ctx: MemoryContext {
                user_id: "u".into(),
                conversation_id: "c".into(),
            },
        })
    }

    /// No cap configured means never rotate. Also covers a disconnected
    /// session, where `connected_at` is `None`.
    #[test]
    fn no_cap_never_rotates() {
        let mut session = a_session();
        session.set_max_session_duration(None);
        session.connected_at = Some(std::time::Instant::now());
        assert!(!session.session_too_old());
    }

    /// A fresh connection is not due, even under the tightest real ceiling.
    #[test]
    fn a_fresh_session_is_not_too_old() {
        let mut session = a_session();
        session.set_max_session_duration(Some(Duration::from_secs(480)));
        session.connected_at = Some(std::time::Instant::now());
        assert!(!session.session_too_old());
    }

    /// The margin is the point: a session must be judged due *before* it
    /// reaches the provider's ceiling, or rotation happens after the remote
    /// close it exists to prevent. A cap below the margin is therefore always
    /// due, which is the correct reading of "rotate 30s early".
    #[test]
    fn the_rotation_margin_makes_a_session_due_before_the_cap() {
        let mut session = a_session();
        session.set_max_session_duration(Some(SESSION_ROTATION_MARGIN));
        session.connected_at = Some(std::time::Instant::now());
        assert!(
            session.session_too_old(),
            "a cap equal to the margin must be due immediately"
        );
    }

    /// A disconnected session has no age and must never look due.
    #[test]
    fn a_disconnected_session_is_never_too_old() {
        let mut session = a_session();
        session.set_max_session_duration(Some(Duration::from_secs(1)));
        session.connected_at = None;
        assert!(!session.session_too_old());
    }

    /// Fragments accumulate into one utterance rather than replacing it --
    /// engines stream transcripts in pieces.
    #[test]
    fn transcript_fragments_are_appended_not_replaced() {
        let mut slot = None;
        Session::append_fragment(&mut slot, "what is");
        Session::append_fragment(&mut slot, "the capital");
        assert_eq!(slot.as_deref(), Some("what is the capital"));
    }

    /// An empty fragment must not add a stray separator.
    #[test]
    fn an_empty_fragment_changes_nothing() {
        let mut slot = Some("intact".to_string());
        Session::append_fragment(&mut slot, "");
        assert_eq!(slot.as_deref(), Some("intact"));
    }
}

#[cfg(test)]
mod prompt_swap_tests {
    use super::*;
    use crate::audio::{AudioFormat, FixtureSource, VecSink};
    use crate::engine::{ConfigProbe, EngineError, EngineId, FakeEngine, ToolCallId};
    use crate::memory::NullMemory;
    use crate::tools::FakeExecutor;
    use async_trait::async_trait;

    fn session_on(engine: Box<dyn S2sEngine>) -> Session {
        let (activation, _tx) = crate::activation::ChannelActivation::new();
        Session::new(SessionDeps {
            source: Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
            sink: Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
            engine,
            executor: std::sync::Arc::new(FakeExecutor::new()),
            memory: std::sync::Arc::new(NullMemory),
            activation: Box::new(activation),
            ctx: MemoryContext {
                user_id: "u".into(),
                conversation_id: "c".into(),
            },
        })
    }

    /// An engine that refuses every `connect()` with a chosen error.
    ///
    /// Deliberately local to this module rather than a failure mode bolted
    /// onto `FakeEngine`: every other session test connects through that
    /// engine, and a shared "sometimes fails" switch would make each of them
    /// depend on a flag none of them set.
    ///
    /// The error is a constructor argument because the two kinds are not
    /// interchangeable here. `Auth` is terminal and sets `Session::terminal`;
    /// `Transport` is transient and does not — and the restore path has to
    /// hold for both, since in either case no connection was established.
    struct RefusingEngine {
        error: EngineError,
    }

    #[async_trait]
    impl S2sEngine for RefusingEngine {
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
            _tx: mpsc::Sender<EngineEvent>,
        ) -> Result<(), EngineError> {
            Err(self.error.clone())
        }
        async fn send_audio(&mut self, _frame: &[i16]) -> Result<(), EngineError> {
            Ok(())
        }
        async fn send_tool_result(
            &mut self,
            _id: ToolCallId,
            _result: crate::tools::ToolResult,
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

    fn a_session_whose_engine_fails_to_connect(error: EngineError) -> Session {
        session_on(Box::new(RefusingEngine { error }))
    }

    fn a_session_with_a_config_probe() -> (Session, ConfigProbe) {
        let engine = FakeEngine::new(vec![]);
        let probe = engine.config_probe();
        (session_on(Box::new(engine)), probe)
    }

    /// The swap must land on the *new* connection, not the one being torn
    /// down — the prompt is only read at `connect()`, so setting it after
    /// would silently apply a session late.
    #[tokio::test]
    async fn a_pending_swap_sets_the_prompt_before_reconnecting() {
        let (mut session, configs) = a_session_with_a_config_probe();
        session.set_system_prompt("original");
        session.connect().await;

        session
            .control
            .request_prompt_swap("You are a pirate.".into(), "pirate".into());
        session.apply_pending_prompt_swap().await;

        // The `SessionConfig` the engine was actually handed is the only
        // evidence of the ordering. Asserting `session.system_prompt` alone
        // would pass just as happily on an implementation that set the prompt
        // *after* connecting -- which is the bug this test is named for, and
        // which would silently leave the user on the old identity for a whole
        // session.
        assert_eq!(
            configs.last().expect("connected").system_prompt.as_deref(),
            Some("You are a pirate."),
            "the new prompt must be in the config the engine connected with"
        );
        assert_eq!(
            configs.count(),
            2,
            "the swap must rotate the connection, not reuse it"
        );

        assert_eq!(session.system_prompt.as_deref(), Some("You are a pirate."));
        assert_eq!(
            *session
                .control
                .subscribe_prompt_swap_outcome()
                .borrow_and_update(),
            Some(PromptSwapOutcome {
                tag: "pirate".into(),
                applied: true
            }),
            "applied only after connect returned"
        );
    }

    #[tokio::test]
    async fn no_pending_swap_does_nothing_at_all() {
        let (mut session, configs) = a_session_with_a_config_probe();
        session.set_system_prompt("original");
        session.connect().await;
        let connects_before = configs.count();

        session.apply_pending_prompt_swap().await;

        assert_eq!(session.system_prompt.as_deref(), Some("original"));
        assert_eq!(configs.count(), connects_before, "must not rotate");
    }

    /// A failed reconnect must leave the user on the identity they had, and
    /// must not report the swap as applied — the shell persists from that
    /// signal.
    #[tokio::test]
    async fn a_failed_reconnect_restores_the_previous_prompt() {
        let mut session = a_session_whose_engine_fails_to_connect(EngineError::Auth("bad".into()));
        session.set_system_prompt("original");

        session
            .control
            .request_prompt_swap("You are a pirate.".into(), "pirate".into());
        session.apply_pending_prompt_swap().await;

        assert_eq!(session.system_prompt.as_deref(), Some("original"));
        assert_eq!(
            *session
                .control
                .subscribe_prompt_swap_outcome()
                .borrow_and_update(),
            Some(PromptSwapOutcome {
                tag: "pirate".into(),
                applied: false
            }),
            "the rollback must be announced, not silent"
        );
    }

    /// The transient case, which the terminal one above does not cover: a
    /// `Transport` failure leaves `terminal` false, so anything gating the
    /// restore on that flag would announce a swap over a connection that was
    /// never established — exactly the half-applied identity the `applied`
    /// signal exists to prevent. No connection is no connection.
    #[tokio::test]
    async fn a_transient_reconnect_failure_also_restores_the_previous_prompt() {
        let mut session =
            a_session_whose_engine_fails_to_connect(EngineError::Transport("reset".into()));
        session.set_system_prompt("original");

        session
            .control
            .request_prompt_swap("You are a pirate.".into(), "pirate".into());
        session.apply_pending_prompt_swap().await;

        assert!(!session.terminal, "a transport failure is not terminal");
        assert_eq!(session.system_prompt.as_deref(), Some("original"));
        assert_eq!(
            *session
                .control
                .subscribe_prompt_swap_outcome()
                .borrow_and_update(),
            Some(PromptSwapOutcome {
                tag: "pirate".into(),
                applied: false
            }),
            "the rollback must be announced, not silent"
        );
    }

    /// A swapped persona must survive a rotation. The max-session-duration
    /// refresh is `disconnect()` + `connect()`, and `connect()` builds its
    /// `SessionConfig` from `self.system_prompt` -- so a swap that only
    /// reached the engine once would silently put the user back on the
    /// persona they left, five minutes after they switched.
    ///
    /// The app deliberately never writes a voice switch to the sidecar, so
    /// this in-process survival is the *whole* of "the switch stuck". Asserts
    /// on the config the engine recorded at the rotation's own connect, not
    /// on `session.system_prompt`: only the config is evidence the engine was
    /// told.
    #[tokio::test]
    async fn a_swapped_prompt_survives_a_rotation() {
        let (mut session, configs) = a_session_with_a_config_probe();
        session.set_system_prompt("original");
        session.connect().await;

        session
            .control
            .request_prompt_swap("You are a pirate.".into(), "pirate".into());
        session.apply_pending_prompt_swap().await;
        let after_swap = configs.count();

        // Exactly what `session_too_old()` drives in the run loop.
        session.disconnect().await;
        session.connect().await;

        assert_eq!(
            configs.count(),
            after_swap + 1,
            "the rotation must have reconnected"
        );
        assert_eq!(
            configs
                .last()
                .expect("reconnected")
                .system_prompt
                .as_deref(),
            Some("You are a pirate."),
            "the rotation must reconnect with the swapped persona, not the original"
        );
    }
}

#[cfg(test)]
mod audio_swap_tests {
    use super::*;
    use crate::audio::{AudioError, AudioFormat, FixtureSource, VecSink};
    use crate::engine::FakeEngine;
    use crate::memory::NullMemory;
    use crate::tools::FakeExecutor;
    use async_trait::async_trait;
    use std::sync::Mutex;

    type Log = Arc<Mutex<Vec<&'static str>>>;

    /// A device that notes when it closes, which for a real device is when
    /// it is dropped.
    struct LoggedSource(Log);

    impl Drop for LoggedSource {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("source dropped");
        }
    }

    #[async_trait]
    impl AudioSource for LoggedSource {
        fn format(&self) -> AudioFormat {
            AudioFormat::mono_pcm16(16_000)
        }
        async fn next_frame(&mut self) -> Option<Vec<i16>> {
            std::future::pending().await
        }
    }

    struct LoggedSink(Log);

    impl Drop for LoggedSink {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("sink dropped");
        }
    }

    #[async_trait]
    impl AudioSink for LoggedSink {
        fn format(&self) -> AudioFormat {
            AudioFormat::mono_pcm16(24_000)
        }
        async fn write(&mut self, _frame: &[i16]) -> Result<(), AudioError> {
            Ok(())
        }
        fn clear(&mut self) {}
    }

    fn session_with(
        source: Box<dyn AudioSource>,
        sink: Box<dyn AudioSink>,
        engine: FakeEngine,
    ) -> Session {
        let (activation, _tx) = crate::activation::ChannelActivation::new();
        Session::new(SessionDeps {
            source,
            sink,
            engine: Box::new(engine),
            executor: Arc::new(FakeExecutor::new()),
            memory: Arc::new(NullMemory),
            activation: Box::new(activation),
            ctx: MemoryContext {
                user_id: "u".into(),
                conversation_id: "c".into(),
            },
        })
    }

    fn a_silent_pair() -> AudioPair {
        (
            Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
            Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
        )
    }

    /// VPIO allows one unit per process, so the old pair must be closed
    /// before the new one is opened -- never the other way round.
    #[tokio::test]
    async fn an_audio_swap_waits_for_the_old_pair_to_drop_before_opening_the_new_one() {
        let log: Log = Arc::default();
        let mut session = session_with(
            Box::new(LoggedSource(log.clone())),
            Box::new(LoggedSink(log.clone())),
            FakeEngine::new(vec![]),
        );
        let opener_log = log.clone();
        session.control.request_audio_swap(AudioSwap {
            open: Some(Box::new(move || {
                opener_log.lock().unwrap().push("opened");
                Ok(a_silent_pair())
            })),
            barge_in_threshold: None,
        });

        assert!(session.apply_pending_audio_swap().await);

        let log = log.lock().unwrap().clone();
        assert_eq!(log.last(), Some(&"opened"), "log: {log:?}");
        assert_eq!(log.len(), 3, "both devices drop, then one open: {log:?}");
        assert!(log.contains(&"source dropped") && log.contains(&"sink dropped"));
    }

    #[tokio::test]
    async fn the_swap_carries_the_barge_in_threshold() {
        let mut session = session_with(
            Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
            Box::new(VecSink::new(AudioFormat::mono_pcm16(24_000))),
            FakeEngine::new(vec![]),
        );
        session.control.request_audio_swap(AudioSwap {
            open: None,
            barge_in_threshold: Some(0.2),
        });
        session.apply_pending_audio_swap().await;
        assert_eq!(session.barge_in_threshold, 0.2);

        session.control.request_audio_swap(AudioSwap {
            open: None,
            barge_in_threshold: None,
        });
        session.apply_pending_audio_swap().await;
        assert_eq!(session.barge_in_threshold, DEFAULT_BARGE_IN_THRESHOLD);
    }

    #[tokio::test]
    async fn a_threshold_only_swap_keeps_the_open_devices() {
        let log: Log = Arc::default();
        let mut session = session_with(
            Box::new(LoggedSource(log.clone())),
            Box::new(LoggedSink(log.clone())),
            FakeEngine::new(vec![]),
        );
        session.control.request_audio_swap(AudioSwap {
            open: None,
            barge_in_threshold: Some(0.2),
        });

        assert!(session.apply_pending_audio_swap().await);

        assert!(
            log.lock().unwrap().is_empty(),
            "a threshold change closed a device"
        );
    }

    #[tokio::test]
    async fn a_swap_while_connected_sends_later_audio_only_to_the_new_sink_without_reconnecting() {
        let old = VecSink::new(AudioFormat::mono_pcm16(24_000));
        let old_probe = old.probe();
        let engine = FakeEngine::new(vec![]);
        let configs = engine.config_probe();
        let mut session = session_with(
            Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![])),
            Box::new(old),
            engine,
        );
        session.connect().await;
        let old_len = old_probe.written().len();

        let new = VecSink::new(AudioFormat::mono_pcm16(24_000));
        let new_probe = new.probe();
        session.control.request_audio_swap(AudioSwap {
            open: Some(Box::new(move || {
                Ok((
                    Box::new(FixtureSource::new(AudioFormat::mono_pcm16(16_000), vec![]))
                        as Box<dyn AudioSource>,
                    Box::new(new) as Box<dyn AudioSink>,
                ))
            })),
            barge_in_threshold: None,
        });
        session.apply_pending_audio_swap().await;
        session
            .handle_event(EngineEvent::AudioChunk(vec![8_000; 480]))
            .await;

        assert!(!new_probe.written().is_empty(), "the new sink got nothing");
        assert_eq!(
            old_probe.written().len(),
            old_len,
            "the old sink still played"
        );
        assert_eq!(configs.count(), 1, "a device swap must not reconnect");
    }
}
