// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Task 13: the persona rotation gap on OpenAI, measured mid-conversation.
//!
//! # Why this is not a stopwatch
//!
//! The plan asks for a human to hold a conversation, ask for a switch, and time
//! from the assistant finishing its confirmation to it speaking as the new
//! persona — five times, by ear. That produces one opaque number whose spread is
//! dominated by the measurer: when the ear decides the confirmation "finished",
//! how long the spoken prompt was, and where the reconnect happened to land
//! relative to the model's own end-of-turn detection. It also cannot be rerun
//! against a change.
//!
//! This measures the same wait with the clock inside the process, which buys
//! three things a stopwatch cannot:
//!
//! 1. **A breakdown.** The gap is not one number. It is `flush_turn` +
//!    `disconnect` + `recall` + `list_tools` + mint + WebRTC + `Ready`, and the
//!    follow-up decision (live `session.update` instead of rotation) turns on
//!    which of those dominates.
//! 2. **The deaf window, which the ear cannot see at all.** Rotation
//!    disconnects. For its whole duration the session is not merely quiet, it is
//!    not listening — anything said into it is gone. A user hears a pause and
//!    assumes they are being heard. That is the number this file reports first.
//! 3. **History sensitivity.** `connect()` replays the conversation through
//!    `memory.recall`, so the gap grows with the conversation. "Mid-conversation"
//!    is the whole point of the task, so history is a parameter here, not a
//!    constant.
//!
//! # What this deliberately does NOT measure
//!
//! The model's willingness to call `switch_persona` at the right moment. That is
//! tool-calling reliability, already covered by `provider_ab.rs`, and folding it
//! in here would add the model's judgement to a latency measurement as noise.
//! The swap is therefore requested directly on `SessionControl` — the identical
//! call `PersonaExecutor::execute` makes, one layer up.
//!
//!   OPENAI_API_KEY=... cargo test -p uia-app --test rotation_gap \
//!       -- --ignored --nocapture --test-threads=1
//!
//! `#[ignore]`d for the frozen test gate: `cargo test` must pass with no network
//! and no audio devices, and this needs both a key and outbound UDP.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use uia_app::personas::{PersonaBook, compose_prompt};
use uia_core::activation::{ActivationEvent, ChannelActivation};
use uia_core::audio::ports::{AudioSink, AudioSource};
use uia_core::audio::{AudioError, AudioFormat, rms};
use uia_core::memory::{
    ConversationMemory, MemoryContext, MemoryError, MemoryItem, MemoryKind, RECALL_ROLE_ASSISTANT,
    RECALL_ROLE_USER, Turn,
};
use uia_core::session::{Session, SessionControl, SessionDeps};
use uia_openai::OpenAiEngine;

/// 20 ms at 24 kHz — what OpenAI TTS returns and OpenAI Realtime consumes.
const TTS_RATE_HZ: u32 = 24_000;
const FRAME_MS: u64 = 20;
const FRAME: usize = (TTS_RATE_HZ as usize) * (FRAME_MS as usize) / 1000;

/// Pinned like every other model id in this project: a floating alias changes
/// the fixture under a rerun and makes the measurement unrepeatable.
const TTS_MODEL: &str = "gpt-4o-mini-tts-2025-12-15";

/// How many switches to time. The plan asks for five.
const ROUNDS: usize = 5;

/// The plan's stated bar, quoted here so the judgement is in the code rather
/// than in someone's memory of the document.
const READS_AS_A_PAUSE: Duration = Duration::from_millis(1500);
const READS_AS_A_FAULT: Duration = Duration::from_millis(3000);

/// A frame with more signal than this is the assistant actually audible, not
/// the near-silent lead-in both providers emit. Same threshold `provider_ab.rs`
/// settled on.
const AUDIBLE: f32 = 0.002;

/// How long the assistant must have been silent before a round's clock starts.
/// Longer than the natural gaps inside a spoken sentence, shorter than the
/// patience of the test.
const QUIET: Duration = Duration::from_millis(800);

// ---------------------------------------------------------------------------
// Audio plumbing
// ---------------------------------------------------------------------------

/// A microphone that never stops.
///
/// Real capture is an endless stream, and both the session's idle logic and
/// OpenAI's `server_vad` assume one — a source that returns `None` ends
/// `run()` with `SourceExhausted` instead of measuring anything. So this yields
/// silence forever, paced at real time, and speaks whatever the test queues.
struct ScriptedSource {
    speech: Arc<Mutex<VecDeque<Vec<i16>>>>,
}

#[async_trait]
impl AudioSource for ScriptedSource {
    fn format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(TTS_RATE_HZ)
    }

    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        // Pacing lives here because this is the only place that runs once per
        // frame regardless of which branch is taken. Blasting frames as fast as
        // the loop will take them gives the server VAD no time base to detect a
        // turn in, which `provider_ab.rs` found the hard way.
        tokio::time::sleep(Duration::from_millis(FRAME_MS)).await;
        let queued = self.speech.lock().unwrap().pop_front();
        Some(queued.unwrap_or_else(|| vec![0i16; FRAME]))
    }
}

/// Notes the instant the assistant first becomes audible after being armed.
///
/// Armed rather than merely "first write": the session speaks several times
/// over a run, and each round needs its own clock. Disarmed by default so that
/// audio still draining from the previous round cannot start this round's
/// measurement.
#[derive(Clone, Default)]
struct AudibleAt {
    at: Arc<Mutex<Option<Instant>>>,
    armed: Arc<Mutex<bool>>,
    /// The last moment anything audible was written, armed or not. This is what
    /// makes "wait until the assistant has stopped talking" possible, and
    /// without it every round after the first would start its clock while the
    /// previous answer was still draining — reporting a few milliseconds and
    /// looking like a triumph. `provider_ab.rs` hit exactly that.
    last: Arc<Mutex<Option<Instant>>>,
}

impl AudibleAt {
    fn arm(&self) {
        *self.at.lock().unwrap() = None;
        *self.armed.lock().unwrap() = true;
    }
    fn seen(&self) -> Option<Instant> {
        *self.at.lock().unwrap()
    }
    /// True when nothing audible has been written for `d`.
    fn quiet_for(&self, d: Duration) -> bool {
        match *self.last.lock().unwrap() {
            None => true,
            Some(t) => t.elapsed() >= d,
        }
    }
}

struct ProbeSink {
    probe: AudibleAt,
}

#[async_trait]
impl AudioSink for ProbeSink {
    fn format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(TTS_RATE_HZ)
    }

    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError> {
        if rms(frame) > AUDIBLE {
            let now = Instant::now();
            *self.probe.last.lock().unwrap() = Some(now);
            if *self.probe.armed.lock().unwrap() {
                let mut at = self.probe.at.lock().unwrap();
                if at.is_none() {
                    *at = Some(now);
                }
            }
        }
        Ok(())
    }

    fn clear(&mut self) {}
}

// ---------------------------------------------------------------------------
// Memory: the conversation the rotation has to carry across
// ---------------------------------------------------------------------------

/// A fixed conversation, replayed on every `connect()`.
///
/// This is the whole reason the task says "mid-conversation": `Session::connect`
/// runs `recall` on the critical path, and a rotation therefore re-sends the
/// entire history to a brand-new session. A measurement taken on turn one would
/// report the best case and miss the growth.
struct ScriptedMemory {
    items: Vec<MemoryItem>,
}

impl ScriptedMemory {
    /// `exchanges` prior user/assistant pairs, in the shape `FileMemory::recall`
    /// produces: one item per half, each tagged with the role that said it.
    fn with_exchanges(exchanges: usize) -> Self {
        let mut items = Vec::with_capacity(exchanges * 2);
        for i in 0..exchanges {
            items.push(MemoryItem {
                content: format!(
                    "Turn {i}: could you remind me what we agreed about the quarterly \
                     rollout schedule and who was going to own the migration?"
                ),
                kind: MemoryKind::Other(RECALL_ROLE_USER.to_string()),
                score: None,
            });
            items.push(MemoryItem {
                content: format!(
                    "Turn {i}: you agreed to stage the rollout over three weeks, and \
                     Priya was going to own the migration."
                ),
                kind: MemoryKind::Other(RECALL_ROLE_ASSISTANT.to_string()),
                score: None,
            });
        }
        Self { items }
    }
}

#[async_trait]
impl ConversationMemory for ScriptedMemory {
    async fn recall(
        &self,
        _ctx: &MemoryContext,
        _q: Option<&str>,
    ) -> Result<Vec<MemoryItem>, MemoryError> {
        Ok(self.items.clone())
    }
    async fn record(&self, _ctx: &MemoryContext, _turn: &Turn) -> Result<(), MemoryError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Speech
// ---------------------------------------------------------------------------

async fn synthesize(api_key: &str, text: &str) -> Vec<i16> {
    let resp = reqwest::Client::new()
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "model": TTS_MODEL,
            "input": text,
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

/// Strip trailing near-silence from a TTS clip.
///
/// `provider_ab.rs` documents why this is not cosmetic: the padding
/// `gpt-4o-mini-tts` leaves at the end of a clip is long enough for the server
/// VAD to call the turn over while the send loop is still streaming, which
/// produces a spectacular-looking time-to-first-audio that is really a clock
/// started after the answer began.
fn trim_trailing_silence(pcm: &[i16]) -> &[i16] {
    const WINDOW: usize = 240; // 10 ms at 24 kHz
    let mut end = pcm.len();
    while end >= WINDOW && rms(&pcm[end - WINDOW..end]) < 0.005 {
        end -= WINDOW;
    }
    &pcm[..end]
}

/// Queue a clip for the source to speak.
fn say(speech: &Arc<Mutex<VecDeque<Vec<i16>>>>, pcm: &[i16]) {
    let mut q = speech.lock().unwrap();
    for f in trim_trailing_silence(pcm).chunks(FRAME) {
        let mut frame = f.to_vec();
        frame.resize(FRAME, 0);
        q.push_back(frame);
    }
}

/// Speak a clip and return the instant the last frame left the source.
///
/// Every response clock in this file starts here rather than at the start of
/// the utterance. The utterance's own length is a property of the fixture, not
/// of the system under test, and folding it in would make a long prompt look
/// like a slow model. What the returned instant does still include, correctly,
/// is the provider's end-of-turn silence detection — the user waits through
/// that too. Same convention as `provider_ab.rs`.
async fn speak(speech: &Arc<Mutex<VecDeque<Vec<i16>>>>, pcm: &[i16]) -> Instant {
    say(speech, pcm);
    loop {
        if speech.lock().unwrap().is_empty() {
            return Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Round {
    persona: String,
    /// Swap requested -> session reported it applied. The session is
    /// disconnected for all of this: the deaf window.
    deaf: Option<Duration>,
    /// End of the user's next utterance -> the new persona is first audible.
    /// Measured on the same clock as `baseline` so the two subtract: anything
    /// above baseline is what the fresh session costs on its first turn, over
    /// and above the rotation itself.
    resume: Option<Duration>,
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs OPENAI_API_KEY and network"]
async fn the_rotation_gap_mid_conversation() {
    let api_key = uia_openai::creds::resolve_api_key_from_env().expect("no OPENAI_API_KEY");

    // Real personas, not toy strings: the composed prompt's length is part of
    // what the reconnect has to carry, and the fixture is the one Task 1 wrote.
    let book: PersonaBook = serde_json::from_str(include_str!("fixtures/personas.json"))
        .expect("the Task 1 persona fixture should parse");
    let enabled: Vec<_> = book.personas.iter().filter(|p| p.enabled).collect();
    assert!(
        enabled.len() >= 2,
        "need at least two enabled personas to switch between; got {}",
        enabled.len()
    );
    let prompts: Vec<(String, String)> = enabled
        .iter()
        .map(|p| {
            (
                p.id.clone(),
                compose_prompt(p, Some("Assistant"), book.allow_agent_switch),
            )
        })
        .collect();

    // A conversation already in progress. Ten exchanges is an ordinary length
    // for a session that has been open a while, and it is the term that a live
    // `session.update` would not have to re-send.
    const EXCHANGES: usize = 10;
    let memory = ScriptedMemory::with_exchanges(EXCHANGES);
    let replayed_chars: usize = memory.items.iter().map(|i| i.content.len()).sum();

    let speech = Arc::new(Mutex::new(VecDeque::new()));
    let probe = AudibleAt::default();
    let (activation, activation_tx) = ChannelActivation::new();
    let control = SessionControl::new();

    let deps = SessionDeps {
        source: Box::new(ScriptedSource {
            speech: speech.clone(),
        }),
        sink: Box::new(ProbeSink {
            probe: probe.clone(),
        }),
        engine: Box::new(OpenAiEngine::from_env().expect("no key")),
        executor: Arc::new(uia_core::tools::fake::FakeExecutor::default()),
        memory: Arc::new(memory),
        activation: Box::new(activation),
        ctx: MemoryContext {
            user_id: "rotation-gap".into(),
            conversation_id: "rotation-gap".into(),
        },
    };

    let mut session = Session::new(deps);
    session.set_control(control.clone());
    session.set_system_prompt(prompts[0].1.clone());
    session.set_transcription(true);
    // The idle disconnect would otherwise fire in the middle of a measurement
    // and be indistinguishable from a rotation.
    session.set_idle_deadline(Duration::ZERO);

    let mut outcomes = control.subscribe_prompt_swap_outcome();
    let runner = tokio::spawn(async move { session.run().await });
    activation_tx
        .send(ActivationEvent::Show)
        .await
        .expect("activation");

    // One real exchange first, so the measurement happens on a session that is
    // genuinely mid-conversation rather than one that has only just connected.
    let opener = synthesize(&api_key, "Hello. In one short sentence, what can you do?").await;
    probe.arm();
    let spoke_at = speak(&speech, &opener).await;
    let baseline = wait_for_audible(&probe, Duration::from_secs(45))
        .await
        .map(|t| t - spoke_at);
    println!("--- baseline time-to-first-audio (no switch): {baseline:?}");
    assert!(
        baseline.is_some(),
        "the session never spoke at all, so nothing below would be a rotation \
         measurement. Check the key, the network, and outbound UDP."
    );

    let follow_up = synthesize(&api_key, "Thanks. And what should I do first?").await;
    let mut rounds = Vec::new();

    for i in 0..ROUNDS {
        // Alternate, so each round is a real change of identity rather than a
        // reconnect onto the prompt already in force.
        let (id, prompt) = &prompts[(i + 1) % prompts.len()];
        let tag = format!("{id}-{i}");

        // Let the previous answer finish before starting this round's clock.
        // Without this the probe would fire on audio that was already playing
        // and report a gap that never happened.
        wait_for_quiet(&probe, QUIET, Duration::from_secs(30)).await;

        probe.arm();
        let t0 = Instant::now();
        control.request_prompt_swap(prompt.clone(), tag.clone());

        // The session reports the swap the moment `connect()` returns having
        // established a stream. Everything before this instant is time the
        // session spent disconnected.
        let deaf = wait_for_outcome(&mut outcomes, &tag, Duration::from_secs(60))
            .await
            .map(|(t, applied)| {
                assert!(applied, "round {i}: the swap failed rather than applying");
                t - t0
            });

        // Rotation leaves the model silent — it has been handed a history and a
        // new identity, but nobody has said anything to it yet. So speak, the
        // way a user would once the pause ends.
        probe.arm();
        let asked_at = speak(&speech, &follow_up).await;
        let resume = wait_for_audible(&probe, Duration::from_secs(45))
            .await
            .map(|t| t - asked_at);

        println!("--- round {i} ({id}): deaf={deaf:?} resume={resume:?}");
        rounds.push(Round {
            persona: id.clone(),
            deaf,
            resume,
        });
    }

    control.request_disconnect();
    runner.abort();

    // ---- report ----
    let mut deaf: Vec<Duration> = rounds.iter().filter_map(|r| r.deaf).collect();
    deaf.sort();
    let mut audible: Vec<Duration> = rounds.iter().filter_map(|r| r.resume).collect();
    audible.sort();

    println!("\n=== Task 13: rotation gap on OpenAI, mid-conversation ===");
    println!("history replayed each rotation: {EXCHANGES} exchanges, {replayed_chars} chars");
    println!("baseline time-to-first-audio (no switch): {baseline:?}");
    for r in &rounds {
        println!(
            "  {:<12} deaf={:>12} resume={:>12}",
            r.persona,
            r.deaf.map(|d| format!("{d:?}")).unwrap_or("-".into()),
            r.resume.map(|d| format!("{d:?}")).unwrap_or("-".into()),
        );
    }
    assert!(
        !deaf.is_empty(),
        "no round produced a rotation measurement; every swap timed out"
    );
    println!(
        "deaf window   min={:?} median={:?} max={:?}",
        deaf[0],
        percentile(&deaf, 0.5),
        deaf[deaf.len() - 1]
    );
    if !audible.is_empty() {
        println!(
            "first turn after  min={:?} median={:?} max={:?}   (baseline {:?})",
            audible[0],
            percentile(&audible, 0.5),
            audible[audible.len() - 1],
            baseline
        );
    }

    // The plan's Step 2, applied in code so the verdict is not left to whoever
    // reads the numbers. The deaf window is judged, not `to_audible`: the
    // latter includes one turn of ordinary response latency, which a live
    // `session.update` would not remove and which `baseline` prices separately.
    let worst = deaf[deaf.len() - 1];
    let verdict = if worst <= READS_AS_A_PAUSE {
        "PAUSE — under the 1.5s bar. Rotation stands; no live-swap follow-up needed."
    } else if worst >= READS_AS_A_FAULT {
        "FAULT — at or over the 3s bar. Open the live `session.update` follow-up \
         for OpenAI and Foundry, keeping rotation for Nova."
    } else {
        "BETWEEN — over 1.5s but under 3s. A judgement call: noticeable, not broken."
    };
    println!("verdict (worst of {ROUNDS}, {worst:?}): {verdict}");
    println!(
        "NOTE: for the whole deaf window the session is disconnected, so speech \
         into it is discarded, not queued. A user hears a pause and assumes they \
         are heard."
    );
}

/// Wait for the session to report on the swap tagged `tag`.
///
/// `changed()` rather than a bare read, for the reason Task 10a recorded: a
/// `watch` holds its last value forever, so re-reading would let an earlier
/// round's outcome answer for this one.
async fn wait_for_outcome(
    outcomes: &mut tokio::sync::watch::Receiver<
        Option<uia_core::session::control::PromptSwapOutcome>,
    >,
    tag: &str,
    timeout: Duration,
) -> Option<(Instant, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        if tokio::time::timeout(remaining, outcomes.changed())
            .await
            .is_err()
        {
            return None;
        }
        let seen = outcomes.borrow_and_update().clone();
        if let Some(o) = seen
            && o.tag == tag
        {
            return Some((Instant::now(), o.applied));
        }
    }
}

/// Block until the assistant has been silent for `quiet`, or `timeout` expires.
///
/// Returning on timeout rather than panicking is deliberate: a round that
/// starts slightly early is a noisy measurement, which the printed table makes
/// visible, whereas a panic here would throw away the four rounds already paid
/// for in API calls and wall time.
async fn wait_for_quiet(probe: &AudibleAt, quiet: Duration, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if probe.quiet_for(quiet) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_audible(probe: &AudibleAt, timeout: Duration) -> Option<Instant> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(t) = probe.seen() {
            return Some(t);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    None
}
