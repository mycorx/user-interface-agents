// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Everything about the HUD's IPC contract that does NOT need `tauri`.
//!
//! `main.rs` cannot be compiled in this sandbox at all (S13: `--features
//! desktop` dies on `libdbus-sys` before webkit2gtk is even reached), so any
//! logic left in it is logic no test can reach. This module is where that
//! logic lives instead: payload shapes, the emit cadence, and what an
//! activation means for the overlay window. `main.rs` keeps only the
//! `tauri` calls themselves.

use crate::config::EngineChoice;
use uia_core::activation::ActivationEvent;
use uia_core::engine::TextTurnSupport;
use uia_core::session::State;

/// The events `src/App.svelte` listens on. Constants because a typo in an
/// event name is silent on both sides: nothing errors, the HUD just never updates.
pub const STATE_EVENT: &str = "uia://state";
pub const LEVEL_EVENT: &str = "uia://level";
/// A rejected `session.update` or other engine error used to only move a
/// state enum — the message itself was discarded, so a schema mismatch (e.g.
/// a voice field the server's schema didn't accept) produced no visible
/// symptom at all. This puts the message somewhere a devtools console shows it.
pub const ERROR_EVENT: &str = "uia://error";

/// Whether the session currently holds an engine connection. Distinct from
/// `STATE_EVENT`, which reports the state *of* a connection: a disconnected
/// session has no state to report and sits at `Idle`, and the UI needs to say
/// why -- deliberately disconnected reads very differently from broken.
pub const CONNECTION_EVENT: &str = "uia://connection";
/// The engine the session actually started with (S18) — `main.rs` emits it
/// once at startup, right alongside the initial `STATE_EVENT`, so the
/// selector control renders the *resolved* choice (persisted override, or
/// the config default) rather than assuming "openai" and being wrong on a
/// restart into Nova.
pub const ENGINE_EVENT: &str = "uia://engine";
/// The running session's typed-turn capability (PLAN.md FD2, S0). Fires
/// whenever the answer changes — an engine switch, or FD3's degrade after a
/// typed turn the engine actually refused. Exactly ONE event for all three
/// engines, paired with exactly one command (`get_text_turn_support`) for the
/// same reason `ENGINE_EVENT` is paired with `get_engine`: an emit at startup
/// can lose the race against the webview registering its listener, so the
/// event is how the HUD learns about *changes* and the command is how it
/// learns the *current* value. S3 is what consumes both; S0 only publishes.
pub const TEXT_TURN_SUPPORT_EVENT: &str = "uia://text-turn-support";

/// One completed exchange, as it is recorded (S4's history strip). The store
/// on disk is the source of truth and `get_history` is how the card reads it;
/// this is only how the HUD learns that a turn happened without polling.
///
/// Either half may be `null`. A typed turn produces an assistant half and no
/// user half, since typed input never becomes a `UserTranscript` -- the HUD
/// already has its own copy of what the user typed, so this is not a gap.
pub const TURN_EVENT: &str = "uia://turn";

/// [`crate::updater::UpdateStatus`], on every change.
pub const UPDATE_EVENT: &str = "uia://update";

/// ~30 Hz, the rate the stage asks for. Capture runs at 50 Hz (20 ms frames),
/// so the meter is deliberately slower than the source: a level bar redrawn
/// faster than a display refreshes costs IPC round trips and buys nothing.
pub const LEVEL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

/// Whether the session can hear the user right now.
///
/// `Idle` and `Connecting` are exactly the states in which `Session::events`
/// is `None` — there is no engine to send audio to, so speech is discarded
/// rather than buffered. Every other state holds a connection, `Speaking`
/// included: the mic is still captured and still sent while the assistant
/// talks, which is what barge-in is.
///
/// This exists because the state word alone does not say it. Rotation — a
/// persona switch, the max-session-duration rotation, every reconnect —
/// disconnects before it reconnects, and `rotation_gap.rs` measured that
/// window at a median 842 ms against live OpenAI. A user who answers into it
/// loses the sentence. "Connecting" describes the socket; this describes the
/// consequence, which is the half the person in front of the HUD needs.
pub fn can_hear(state: State) -> bool {
    !matches!(state, State::Idle | State::Connecting)
}

/// `{ "state": "Listening", "hearing": true }` — the shape `App.svelte`
/// destructures.
///
/// `hearing` is derived rather than sent separately so the two can never
/// disagree: one event, one moment, one answer to "what is the session doing
/// and can it hear me".
pub fn state_payload(state: State) -> serde_json::Value {
    serde_json::json!({ "state": crate::state_label(state), "hearing": can_hear(state) })
}

/// `{ "rms": 0.42 }`, clamped. The HUD clamps too, but a NaN would survive
/// `Math.max/min` in JS and render as an empty meter, so it is cleaned here
/// where it can be tested.
pub fn level_payload(rms: f32) -> serde_json::Value {
    let clean = if rms.is_finite() {
        rms.clamp(0.0, 1.0)
    } else {
        0.0
    };
    serde_json::json!({ "rms": clean })
}

/// `{ "user": "...", "assistant": "..." }`, either half possibly `null`.
///
/// Deliberately not filtered here. A turn with one half is what was recorded
/// and what the history card will show, so suppressing it would make the live
/// strip and the stored history disagree.
pub fn turn_payload(turn: &uia_core::memory::Turn) -> serde_json::Value {
    serde_json::json!({ "user": turn.user, "assistant": turn.assistant })
}

/// `{ "message": "protocol error: ..." }` — the shape `App.svelte` destructures.
pub fn error_payload(message: &str) -> serde_json::Value {
    serde_json::json!({ "message": message })
}

/// `{ "connected": true }` — the shape `HudCard.svelte` destructures.
pub fn connection_payload(connected: bool) -> serde_json::Value {
    serde_json::json!({ "connected": connected })
}

/// `{ "engine": "openai" }` or `{ "engine": "bedrock" }` — the shape `App.svelte`
/// destructures to preselect the control.
pub fn engine_payload(choice: EngineChoice) -> serde_json::Value {
    serde_json::json!({ "engine": choice.as_wire() })
}

/// `{ "support": "supported" | "unsupported" | "unknown", "reason": null | "..." }`.
///
/// Deliberately NOT `TextTurnSupport`'s own serde shape: the Rust enum's
/// externally-tagged form (`{"Unsupported": "..."}`) makes the frontend
/// branch on which key is present before it can read either field, and this
/// payload is destructured in a Svelte template. A flat discriminant plus an
/// always-present `reason` slot reads the same way for all three states —
/// same reasoning as `state_payload`'s plain string.
pub fn text_turn_support_payload(support: &TextTurnSupport) -> serde_json::Value {
    let label = match support {
        TextTurnSupport::Supported => "supported",
        TextTurnSupport::Unsupported(_) => "unsupported",
        TextTurnSupport::Unknown => "unknown",
    };
    serde_json::json!({ "support": label, "reason": support.reason() })
}

/// What an [`ActivationEvent`] means for the overlay window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    Show,
    Hide,
    Toggle,
    /// Session-level only (barge-in): the window must not move.
    None,
}

pub fn window_action_for(event: ActivationEvent) -> WindowAction {
    match event {
        ActivationEvent::Show => WindowAction::Show,
        ActivationEvent::Hide => WindowAction::Hide,
        ActivationEvent::Toggle => WindowAction::Toggle,
        ActivationEvent::Interrupt => WindowAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_payload_is_the_shape_the_hud_destructures() {
        // App.svelte reads `event.payload.state` as a string. A renamed key
        // fails silently on both sides — hence a test, not a comment.
        assert_eq!(
            state_payload(State::ToolRunning),
            serde_json::json!({ "state": "ToolRunning", "hearing": true })
        );
    }

    #[test]
    fn the_state_payload_says_whether_the_session_can_hear() {
        // The deaf window: rotation disconnects before it reconnects, so for
        // the whole gap speech is discarded rather than buffered. The state
        // word alone does not say that — "Connecting" describes the socket,
        // not the consequence — so the payload carries the consequence too.
        assert_eq!(
            state_payload(State::Connecting),
            serde_json::json!({ "state": "Connecting", "hearing": false })
        );
        assert_eq!(
            state_payload(State::Listening),
            serde_json::json!({ "state": "Listening", "hearing": true })
        );
    }

    #[test]
    fn only_a_session_holding_a_connection_can_hear() {
        // `Idle` and `Connecting` are exactly the states in which
        // `Session::events` is `None`, i.e. there is no engine to send audio
        // to. Everything else holds a connection.
        for deaf in [State::Idle, State::Connecting] {
            assert!(!can_hear(deaf), "{deaf:?} holds no connection");
        }
        // `Speaking` included, deliberately: the mic is still captured and
        // still sent while the assistant talks — that is what barge-in is.
        for hearing in [
            State::Listening,
            State::Thinking,
            State::ToolRunning,
            State::Speaking,
            State::Interrupting,
        ] {
            assert!(can_hear(hearing), "{hearing:?} is connected");
        }
    }

    #[test]
    fn the_level_payload_is_clamped_and_never_nan() {
        assert_eq!(level_payload(0.5), serde_json::json!({ "rms": 0.5 }));
        assert_eq!(level_payload(1.7), serde_json::json!({ "rms": 1.0 }));
        assert_eq!(level_payload(-0.2), serde_json::json!({ "rms": 0.0 }));
        // JS's Math.max(0, Math.min(1, NaN)) is NaN, which renders as a blank
        // meter rather than a silent zero — so it is cleaned on this side.
        assert_eq!(level_payload(f32::NAN), serde_json::json!({ "rms": 0.0 }));
    }

    #[test]
    fn the_turn_payload_is_the_shape_the_hud_destructures() {
        // `HudCard.svelte` reads `event.payload.user` and
        // `event.payload.assistant`. A renamed key is silent on both sides:
        // the strip would simply stop growing.
        assert_eq!(
            turn_payload(&uia_core::memory::Turn {
                user: Some("what is the retry policy".into()),
                assistant: Some("Five attempts, backing off.".into()),
            }),
            serde_json::json!({
                "user": "what is the retry policy",
                "assistant": "Five attempts, backing off.",
            })
        );
    }

    /// A one-sided exchange must survive as `null`, not be dropped or turned
    /// into an empty string.
    ///
    /// Both halves happen in normal use. A typed turn produces an assistant
    /// half and no user half, since typed input never becomes a
    /// `UserTranscript` — suppressing those would silently discard every reply
    /// to anything the user types. And everything recorded before
    /// transcription was enabled has an assistant half alone, which the
    /// history card shows; hiding it live would make the strip and the card
    /// disagree about the same conversation.
    #[test]
    fn a_one_sided_turn_keeps_its_missing_half_as_null() {
        assert_eq!(
            turn_payload(&uia_core::memory::Turn {
                user: None,
                assistant: Some("Done.".into()),
            }),
            serde_json::json!({ "user": null, "assistant": "Done." })
        );
        assert_eq!(
            turn_payload(&uia_core::memory::Turn {
                user: Some("go on".into()),
                assistant: None,
            }),
            serde_json::json!({ "user": "go on", "assistant": null })
        );
    }

    #[test]
    fn the_event_names_are_distinct() {
        // Every one of these is a string literal matched by a string literal
        // in the webview, so a copy-paste collision would route two different
        // signals to one listener and show up as a UI that updates at the
        // wrong moments rather than as an error.
        let names = [
            STATE_EVENT,
            LEVEL_EVENT,
            ERROR_EVENT,
            CONNECTION_EVENT,
            ENGINE_EVENT,
            TEXT_TURN_SUPPORT_EVENT,
            TURN_EVENT,
        ];
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate event name in {names:?}"
        );
    }

    #[test]
    fn the_error_payload_is_the_shape_the_hud_destructures() {
        assert_eq!(
            error_payload("protocol error: Unknown parameter: 'voice'"),
            serde_json::json!({ "message": "protocol error: Unknown parameter: 'voice'" })
        );
    }

    #[test]
    fn the_engine_payload_is_the_shape_the_hud_destructures() {
        assert_eq!(
            engine_payload(EngineChoice::OpenAi),
            serde_json::json!({ "engine": "openai" })
        );
        assert_eq!(
            engine_payload(EngineChoice::Bedrock),
            serde_json::json!({ "engine": "bedrock" })
        );
        assert_eq!(
            engine_payload(EngineChoice::Foundry),
            serde_json::json!({ "engine": "foundry" })
        );
    }

    #[test]
    fn the_text_turn_support_payload_is_flat_for_all_three_states() {
        assert_eq!(
            text_turn_support_payload(&TextTurnSupport::Supported),
            serde_json::json!({ "support": "supported", "reason": null })
        );
        assert_eq!(
            text_turn_support_payload(&TextTurnSupport::Unknown),
            serde_json::json!({ "support": "unknown", "reason": null })
        );
        // The reason is the string S3 renders next to the disabled control
        // (FD6), so it must survive the trip verbatim.
        assert_eq!(
            text_turn_support_payload(&TextTurnSupport::Unsupported("speak instead".into())),
            serde_json::json!({ "support": "unsupported", "reason": "speak instead" })
        );
    }

    #[test]
    fn the_update_event_name_is_namespaced_like_the_others() {
        assert_eq!(UPDATE_EVENT, "uia://update");
    }

    #[test]
    fn the_capability_event_name_is_namespaced_like_the_others() {
        // A typo in an event name is silent on both sides — the same reason
        // these are constants at all.
        assert_eq!(TEXT_TURN_SUPPORT_EVENT, "uia://text-turn-support");
        for other in [STATE_EVENT, LEVEL_EVENT, ERROR_EVENT, ENGINE_EVENT] {
            assert_ne!(TEXT_TURN_SUPPORT_EVENT, other);
        }
    }

    #[test]
    fn the_meter_is_throttled_below_the_capture_rate() {
        assert!(
            LEVEL_INTERVAL >= std::time::Duration::from_millis(30),
            "at or under ~33 Hz"
        );
        assert!(
            LEVEL_INTERVAL < std::time::Duration::from_millis(50),
            "still visibly live"
        );
    }

    #[test]
    fn barge_in_never_moves_the_window() {
        assert_eq!(
            window_action_for(ActivationEvent::Interrupt),
            WindowAction::None,
            "talking over the assistant must not hide or show the overlay"
        );
        assert_eq!(window_action_for(ActivationEvent::Show), WindowAction::Show);
        assert_eq!(window_action_for(ActivationEvent::Hide), WindowAction::Hide);
        assert_eq!(
            window_action_for(ActivationEvent::Toggle),
            WindowAction::Toggle
        );
    }
}
