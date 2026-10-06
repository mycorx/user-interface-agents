// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! A handle onto a *running* `Session`.
//!
//! The session is owned by whatever task drives `Session::run` — in the
//! desktop shell, a `tauri::async_runtime::spawn`ed task that holds it by
//! value for the life of the process. Nothing outside that task can reach it,
//! which is fine for a session that only ever needed to be started once
//! (PLAN.md FD6) and not fine the moment a UI control has to change how the
//! running session behaves. This is that missing handle.
//!
//! It is deliberately **not** a command channel for the capture gate — the
//! push-to-talk mic button has to be read on the capture path, once per
//! ~20 ms frame, from inside `pump_audio`; a message the run loop drains
//! between frames would add a frame of latency and, worse, could not be read
//! at all while the loop is parked awaiting engine traffic. An
//! `Arc<AtomicBool>` is readable from inside the loop at any instant, with no
//! await point and no lock.
//!
//! A typed turn (PLAN.md S4) is different: it is rare, user-paced (as fast as
//! someone can type and hit send) and needs to carry a payload, so it *is* a
//! channel, added here alongside the flag rather than turning the flag into
//! a message. The sending half is shared like `capture_enabled` — every
//! clone submits into the same queue — but the receiving half can only be
//! *taken* once, by whichever `Session` ends up driving this handle, so two
//! sessions sharing a stale handle can never both drain it.

use crate::engine::TextTurnSupport;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

/// How many typed turns may queue before `submit_text_turn` starts rejecting
/// new ones. Generous relative to how fast a person can type-and-send
/// repeatedly; a queue this deep only fills if the session itself is stuck.
const TEXT_TURN_QUEUE_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TypedTurnError {
    /// The queue is full, or nothing is left to drain it (the session ended
    /// or never took the receiver). Either way, submitting again later is
    /// the wrong fix — this is not a transient network failure.
    #[error("this session is not accepting typed turns right now")]
    Unavailable,
}

/// A system prompt the shell wants the running session to adopt, and an
/// opaque `tag` handed back once it has actually been applied.
///
/// `uia-core` deliberately does not know what a persona is: it carries a
/// string and a label for a string. Everything that decides *which* prompt
/// belongs to *which* identity lives in the shell.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptSwap {
    pub prompt: String,
    pub tag: String,
}

/// What became of a requested swap: the `tag` it was requested under, and
/// whether the session actually landed on it.
///
/// One value rather than two channels on purpose. A subscriber that had to
/// watch "applied" and "failed" separately would have to correlate them to
/// answer the only question it ever asks -- *did the swap I am waiting on
/// land?* -- and would get that correlation wrong the moment two swaps share
/// a tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSwapOutcome {
    pub tag: String,
    pub applied: bool,
}

/// Cloneable, `Send + Sync` handle onto a running session. Every clone shares
/// one set of flags, so the copy `.manage()`d by the shell and the copy held
/// by the session are the same switch.
#[derive(Clone)]
pub struct SessionControl {
    capture_enabled: Arc<AtomicBool>,
    text_tx: mpsc::Sender<String>,
    /// `Some` until a `Session` calls `take_text_rx()`. Wrapped for a one-shot
    /// handoff, not for ongoing access — nothing reads through this lock on
    /// any hot path.
    text_rx: Arc<Mutex<Option<mpsc::Receiver<String>>>>,
    /// The running session's typed-turn capability (PLAN.md FD2). A `watch`
    /// rather than a broadcast because this is a *state*, not a sequence:
    /// a shell that reloads its webview needs the current value, and a shell
    /// that stayed up needs to be told when it changes — `watch` is exactly
    /// both, and coalescing intermediate values costs nothing here (unlike
    /// `state_tx`, where the order IS the payload).
    text_turn_support: Arc<watch::Sender<TextTurnSupport>>,
    /// Whether the shell wants an engine connection at all.
    ///
    /// A `watch`, not an `AtomicBool` like `capture_enabled`, and the reason
    /// is the difference between the two gates. The capture gate is read once
    /// per captured frame, so there is always a next frame to notice it. A
    /// disconnected session has no frames: the run loop is parked, and only
    /// something awaitable can wake it.
    connection_wanted: Arc<watch::Sender<bool>>,
    /// `Some` while a swap is waiting for the run loop. A `Mutex<Option<_>>`
    /// rather than a `watch`, because this is a one-shot the loop *consumes*
    /// — a `watch` would re-deliver the same swap on every poll, and the
    /// session would rotate forever.
    prompt_swap: Arc<Mutex<Option<PromptSwap>>>,
    /// What became of the last swap the session took -- applied, or failed
    /// and rolled back. A `watch` because this is a state the shell may miss
    /// the moment of: the shell reconciles its own idea of the active
    /// persona from here, lazily, and must be able to read the current value
    /// after the fact rather than only at the instant it changed.
    prompt_swap_outcome: Arc<watch::Sender<Option<PromptSwapOutcome>>>,
}

impl std::fmt::Debug for SessionControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionControl")
            .field("capture_enabled", &self.capture_enabled())
            .field("connection_wanted", &self.connection_wanted())
            .field("text_turn_support", &self.text_turn_support())
            .finish_non_exhaustive()
    }
}

impl SessionControl {
    /// A handle with capture **enabled** — the behaviour that predates this
    /// handle. A session whose shell never touches the mic gate must keep
    /// forwarding audio exactly as before.
    pub fn new() -> Self {
        let (text_tx, text_rx) = mpsc::channel(TEXT_TURN_QUEUE_DEPTH);
        Self {
            capture_enabled: Arc::new(AtomicBool::new(true)),
            text_tx,
            text_rx: Arc::new(Mutex::new(Some(text_rx))),
            // `Unknown` until a `Session` publishes its engine's answer: a
            // handle nobody has attached a session to has nothing to report,
            // and FD3 says an unmade claim never costs the user their input.
            text_turn_support: Arc::new(watch::channel(TextTurnSupport::Unknown).0),
            // Wanted by default, for the same reason capture is: a shell
            // that never touches this gate must behave exactly as before.
            connection_wanted: Arc::new(watch::channel(true).0),
            prompt_swap: Arc::new(Mutex::new(None)),
            prompt_swap_outcome: Arc::new(watch::channel(None).0),
        }
    }

    /// Gate capture *forwarding*. False means captured frames are still read
    /// from the device and still measured, but never reach the engine
    /// (PLAN.md FD6/FD15) — this does not activate, deactivate, connect or
    /// disconnect anything.
    pub fn set_capture_enabled(&self, enabled: bool) {
        // `Relaxed` is right: this flag has no happens-before relationship
        // with any other memory the capture path reads. The only guarantee
        // anyone needs is that the write becomes visible, which every
        // ordering gives.
        self.capture_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Whether captured frames are currently forwarded to the engine.
    pub fn capture_enabled(&self) -> bool {
        self.capture_enabled.load(Ordering::Relaxed)
    }

    /// Ask the running session to drop its engine connection. Returns
    /// immediately; the session acts on it at its next opportunity.
    ///
    /// Unlike `set_capture_enabled` this is not free -- reconnecting costs a
    /// fresh connect -- so it is for deliberate gestures (opening Settings, an
    /// idle timeout) rather than anything per-frame.
    pub fn request_disconnect(&self) {
        self.connection_wanted.send_replace(false);
    }

    /// Ask the running session to (re)establish its engine connection.
    pub fn request_connect(&self) {
        self.connection_wanted.send_replace(true);
    }

    /// Whether a connection is currently wanted. Read by the shell to render
    /// its own controls; the session watches the channel instead.
    pub fn connection_wanted(&self) -> bool {
        *self.connection_wanted.borrow()
    }

    /// The run loop's wake-up: parked while disconnected, woken when the
    /// shell asks for a connection again.
    pub fn subscribe_connection_wanted(&self) -> watch::Receiver<bool> {
        self.connection_wanted.subscribe()
    }

    /// Queue a typed turn for the running session to send. Infallible-ish by
    /// construction for a healthy session — the queue is deep relative to
    /// how fast a person can submit — but never blocks: a full or abandoned
    /// queue is reported back rather than stalling the caller (the Tauri
    /// command thread).
    pub fn submit_text_turn(&self, text: impl Into<String>) -> Result<(), TypedTurnError> {
        self.text_tx
            .try_send(text.into())
            .map_err(|_| TypedTurnError::Unavailable)
    }

    /// Hand the receiving half to whoever will drain it. Exactly one caller
    /// succeeds; every later call (a stale handle, a second session) gets
    /// `None`. The taker owns the `Receiver` outright afterward — no lock on
    /// the per-frame path.
    pub fn take_text_rx(&self) -> Option<mpsc::Receiver<String>> {
        self.text_rx.lock().expect("text_rx mutex poisoned").take()
    }

    /// The running session's current typed-turn capability (PLAN.md FD2).
    /// Always readable — the shell's `get_text_turn_support` command answers
    /// straight out of here, so a reloaded webview never has to guess or wait
    /// for the next event.
    pub fn text_turn_support(&self) -> TextTurnSupport {
        self.text_turn_support.borrow().clone()
    }

    /// Be told when it changes. Subscribe before the session runs; the
    /// receiver also carries the value current at subscription time.
    pub fn subscribe_text_turn_support(&self) -> watch::Receiver<TextTurnSupport> {
        self.text_turn_support.subscribe()
    }

    /// Publish the engine's own answer. Crate-visible on purpose: FD2 says
    /// every engine answers the same question the same way, and letting a UI
    /// surface write its own answer here is precisely the ad-hoc-per-surface
    /// situation this type replaces. Called by `Session` when the engine
    /// changes, never per turn.
    ///
    /// A no-op when the value is unchanged, so a subscriber only wakes on a
    /// real transition and the shell never emits a redundant event.
    pub(crate) fn publish_text_turn_support(&self, support: TextTurnSupport) {
        self.text_turn_support.send_if_modified(|current| {
            if *current == support {
                false
            } else {
                *current = support;
                true
            }
        });
    }

    /// FD3's degrade transition: a typed turn that actually failed demotes
    /// this session to `Unsupported`, with the failure itself as the reason.
    /// Crate-visible for the same reason as `publish_text_turn_support`, and
    /// living here rather than in each engine or each UI surface is what
    /// makes the rule uniform.
    ///
    /// Sticky within a session: once demoted, nothing re-reads the engine's
    /// optimistic answer until the engine itself changes (`Session::
    /// switch_engine`). A capability that flapped back to `Unknown` after a
    /// proven failure would re-enable a control that is known not to work.
    pub(crate) fn degrade_text_turn_support(&self, reason: impl Into<String>) {
        self.publish_text_turn_support(TextTurnSupport::Unsupported(reason.into()));
    }

    /// Ask the running session to adopt `prompt` and rotate its connection.
    /// Returns immediately; the session acts at its next pump.
    ///
    /// A pending swap that has not been taken yet is *replaced*, not queued:
    /// two switches before the loop gets a turn means the user changed their
    /// mind, and rotating twice would cost two reconnects to reach the same
    /// place.
    pub fn request_prompt_swap(&self, prompt: String, tag: String) {
        *self.prompt_swap.lock().expect("prompt swap mutex") = Some(PromptSwap { prompt, tag });
    }

    /// Consumes the pending swap, if any. Called by the run loop only.
    pub fn take_prompt_swap(&self) -> Option<PromptSwap> {
        self.prompt_swap.lock().expect("prompt swap mutex").take()
    }

    /// Announce that a swap has been applied and the session reconnected on
    /// it. Called by the run loop *after* a successful `connect()`, which is
    /// what makes it safe for the shell to adopt the new identity here:
    /// a failed connect never reaches this call.
    pub fn note_prompt_swap_applied(&self, tag: String) {
        self.publish_prompt_swap_outcome(tag, true);
    }

    /// Announce that a swap was taken but never landed -- the session has
    /// been put back on the prompt it had. Called by the run loop when the
    /// post-swap `connect()` established nothing.
    ///
    /// Reporting the failure is the whole of the contract: the swap is *not*
    /// re-queued here. A transient failure that retried itself would rotate
    /// the session in a loop, so recovery is the user asking again.
    pub fn note_prompt_swap_failed(&self, tag: String) {
        self.publish_prompt_swap_outcome(tag, false);
    }

    /// `send_replace`, not `send_if_modified`: two identical outcomes in a
    /// row are two real events (the same persona requested twice, failing
    /// twice), and a subscriber that reconciles on change detection must
    /// wake for the second one.
    fn publish_prompt_swap_outcome(&self, tag: String, applied: bool) {
        self.prompt_swap_outcome
            .send_replace(Some(PromptSwapOutcome { tag, applied }));
    }

    /// Be told what became of the swaps this handle requested. The receiver
    /// also carries the value current at subscription time, so a subscriber
    /// that reads it lazily never misses an outcome -- only intermediate
    /// ones, which is exactly the coalescing `request_prompt_swap`'s own
    /// latest-wins semantics already impose.
    pub fn subscribe_prompt_swap_outcome(&self) -> watch::Receiver<Option<PromptSwapOutcome>> {
        self.prompt_swap_outcome.subscribe()
    }
}

impl Default for SessionControl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_handle_forwards_capture() {
        assert!(SessionControl::new().capture_enabled());
    }

    #[test]
    fn clones_share_one_switch() {
        // The whole point: the shell's copy and the session's copy are not
        // two independent flags that could drift.
        let a = SessionControl::new();
        let b = a.clone();
        a.set_capture_enabled(false);
        assert!(!b.capture_enabled());
        b.set_capture_enabled(true);
        assert!(a.capture_enabled());
    }

    #[tokio::test]
    async fn a_submitted_turn_reaches_whoever_took_the_receiver() {
        let control = SessionControl::new();
        let mut rx = control.take_text_rx().expect("first take succeeds");
        control.submit_text_turn("hello").unwrap();
        assert_eq!(rx.recv().await, Some("hello".to_string()));
    }

    #[test]
    fn the_receiver_can_only_be_taken_once() {
        let control = SessionControl::new();
        assert!(control.take_text_rx().is_some());
        assert!(control.take_text_rx().is_none());
    }

    #[tokio::test]
    async fn clones_share_the_same_queue() {
        let a = SessionControl::new();
        let b = a.clone();
        let mut rx = a.take_text_rx().unwrap();
        b.submit_text_turn("from b").unwrap();
        assert_eq!(rx.recv().await, Some("from b".to_string()));
    }

    #[test]
    fn a_fresh_handle_makes_no_capability_claim() {
        // FD3: `Unknown`, not `Unsupported` — a handle no session has
        // attached to yet has nothing to report, and an unmade claim must
        // never cost the user their input.
        assert_eq!(
            SessionControl::new().text_turn_support(),
            TextTurnSupport::Unknown
        );
    }

    #[test]
    fn clones_share_one_capability() {
        // Same reasoning as `clones_share_one_switch`: the shell's copy and
        // the session's copy must not be two answers that can disagree.
        let a = SessionControl::new();
        let b = a.clone();
        a.publish_text_turn_support(TextTurnSupport::Supported);
        assert_eq!(b.text_turn_support(), TextTurnSupport::Supported);
        b.degrade_text_turn_support("the engine refused it");
        assert_eq!(
            a.text_turn_support(),
            TextTurnSupport::Unsupported("the engine refused it".into())
        );
    }

    #[test]
    fn republishing_the_same_answer_does_not_wake_a_subscriber() {
        // The shell emits a Tauri event per wake-up, so a no-op send would
        // become a redundant IPC round trip on every engine reconnect.
        let control = SessionControl::new();
        let mut rx = control.subscribe_text_turn_support();
        control.publish_text_turn_support(TextTurnSupport::Unknown);
        assert!(!rx.has_changed().unwrap(), "no transition, no wake-up");

        control.publish_text_turn_support(TextTurnSupport::Supported);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), TextTurnSupport::Supported);
    }

    #[test]
    fn a_prompt_swap_is_taken_exactly_once() {
        let control = SessionControl::new();
        assert!(
            control.take_prompt_swap().is_none(),
            "nothing pending by default"
        );

        control.request_prompt_swap("You are a pirate.".into(), "pirate".into());
        let swap = control.take_prompt_swap().expect("pending");
        assert_eq!(swap.prompt, "You are a pirate.");
        assert_eq!(swap.tag, "pirate");
        assert!(control.take_prompt_swap().is_none(), "consumed");
    }

    #[test]
    fn a_second_request_replaces_an_unconsumed_first() {
        // Two switches before the run loop gets a turn: the later one is
        // what the user asked for most recently, and applying both would
        // rotate twice for no reason.
        let control = SessionControl::new();
        control.request_prompt_swap("a".into(), "first".into());
        control.request_prompt_swap("b".into(), "second".into());
        assert_eq!(control.take_prompt_swap().unwrap().tag, "second");
        assert!(control.take_prompt_swap().is_none());
    }

    #[test]
    fn applied_is_reported_only_after_the_session_says_so() {
        let control = SessionControl::new();
        let mut outcome = control.subscribe_prompt_swap_outcome();
        assert_eq!(*outcome.borrow_and_update(), None);

        control.request_prompt_swap("a".into(), "focus".into());
        assert_eq!(
            *outcome.borrow_and_update(),
            None,
            "requesting is not applying"
        );

        control.note_prompt_swap_applied("focus".into());
        assert_eq!(
            *outcome.borrow_and_update(),
            Some(PromptSwapOutcome {
                tag: "focus".into(),
                applied: true
            })
        );
    }

    /// The failure half of the same channel. One watch rather than two means
    /// a subscriber reads a single value and knows both *which* swap it is
    /// hearing about and whether it landed -- it never has to correlate an
    /// "applied" stream against a "failed" one.
    #[test]
    fn a_failed_swap_is_reported_on_the_same_channel() {
        let control = SessionControl::new();
        let mut outcome = control.subscribe_prompt_swap_outcome();

        control.note_prompt_swap_failed("focus".into());
        assert_eq!(
            *outcome.borrow_and_update(),
            Some(PromptSwapOutcome {
                tag: "focus".into(),
                applied: false
            })
        );

        // And a later success on the same tag is a fresh, distinguishable
        // wake-up: the decorator retries by the user asking again, and must
        // see the second answer rather than a value it has already consumed.
        control.note_prompt_swap_applied("focus".into());
        assert!(outcome.has_changed().unwrap());
        assert_eq!(
            *outcome.borrow_and_update(),
            Some(PromptSwapOutcome {
                tag: "focus".into(),
                applied: true
            })
        );
    }

    #[test]
    fn clones_share_one_swap_slot() {
        let control = SessionControl::new();
        let clone = control.clone();
        control.request_prompt_swap("a".into(), "t".into());
        assert!(
            clone.take_prompt_swap().is_some(),
            "a clone is the same switch"
        );
    }

    #[test]
    fn submitting_with_nobody_left_to_drain_it_is_reported_not_silently_dropped() {
        let control = SessionControl::new();
        let rx = control.take_text_rx().unwrap();
        drop(rx);
        assert_eq!(
            control.submit_text_turn("too late"),
            Err(TypedTurnError::Unavailable)
        );
    }
}
