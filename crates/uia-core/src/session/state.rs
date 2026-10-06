// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The conversation lifecycle as a pure function.
//!
//! `transition` takes no `self`, no clock, and does no I/O, which is what makes
//! the whole machine table-testable. Every behaviour built on top of it in
//! later stages (barge-in, tool dispatch, backoff reconnection) reduces to
//! feeding it a `SessionInput`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Connecting,
    Listening,
    Thinking,
    ToolRunning,
    Speaking,
    Interrupting,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionInput {
    Activate,
    Deactivate,
    EngineReady,
    SpeechStarted,
    SpeechEnded,
    /// Local energy detector fired while the assistant was speaking.
    UserSpoke,
    /// Engine acknowledged the interrupt.
    Interrupted,
    ToolCalled,
    ToolFinished,
    /// Auth failures and the like: do not reconnect.
    TerminalError,
    /// Network drops: reconnect with backoff.
    TransientError,
}

/// Pure transition function. `None` means the input is not valid in this state
/// and should be logged and dropped rather than acted on.
pub fn transition(state: State, input: &SessionInput) -> Option<State> {
    use SessionInput as I;
    use State as S;

    // Deactivate and terminal errors unwind from anywhere active.
    match input {
        I::Deactivate | I::TerminalError if state != S::Idle => return Some(S::Idle),
        I::TransientError if state != S::Idle => return Some(S::Connecting),
        _ => {}
    }

    match (state, input) {
        (S::Idle, I::Activate) => Some(S::Connecting),
        (S::Connecting, I::EngineReady) => Some(S::Listening),

        (S::Listening | S::Thinking, I::SpeechStarted) => Some(S::Speaking),
        (S::Speaking, I::SpeechEnded) => Some(S::Listening),

        (S::Speaking, I::UserSpoke) => Some(S::Interrupting),
        (S::Interrupting, I::Interrupted) => Some(S::Listening),
        // Some providers never acknowledge; a following SpeechEnded also clears it.
        (S::Interrupting, I::SpeechEnded) => Some(S::Listening),

        (S::Listening | S::Speaking | S::Thinking, I::ToolCalled) => Some(S::ToolRunning),
        (S::ToolRunning, I::ToolFinished) => Some(S::Thinking),

        _ => None,
    }
}

/// Engine selection may only change while fully idle. This one guard removes
/// every hard mid-flight teardown case at no runtime cost.
///
/// Note this is a function of state *alone* — deliberately. `PLAN.md` freezes
/// the fallback as manually selected, never automatic, so nothing here needs to
/// know which engine is running or why the user is switching.
pub fn can_switch_engine(state: State) -> bool {
    state == State::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions_follow_the_specified_lifecycle() {
        let cases = [
            (State::Idle, SessionInput::Activate, Some(State::Connecting)),
            (
                State::Connecting,
                SessionInput::EngineReady,
                Some(State::Listening),
            ),
            (
                State::Listening,
                SessionInput::SpeechStarted,
                Some(State::Speaking),
            ),
            (
                State::Speaking,
                SessionInput::SpeechEnded,
                Some(State::Listening),
            ),
            (
                State::Speaking,
                SessionInput::UserSpoke,
                Some(State::Interrupting),
            ),
            (
                State::Interrupting,
                SessionInput::Interrupted,
                Some(State::Listening),
            ),
            (
                State::Listening,
                SessionInput::ToolCalled,
                Some(State::ToolRunning),
            ),
            (
                State::Speaking,
                SessionInput::ToolCalled,
                Some(State::ToolRunning),
            ),
            (
                State::ToolRunning,
                SessionInput::ToolFinished,
                Some(State::Thinking),
            ),
            (
                State::Thinking,
                SessionInput::SpeechStarted,
                Some(State::Speaking),
            ),
            (
                State::Listening,
                SessionInput::Deactivate,
                Some(State::Idle),
            ),
            (State::Speaking, SessionInput::Deactivate, Some(State::Idle)),
        ];
        for (from, input, want) in cases {
            assert_eq!(transition(from, &input), want, "{from:?} + {input:?}");
        }
    }

    #[test]
    fn invalid_transitions_are_rejected_rather_than_silently_ignored() {
        // Returning None lets the session log the anomaly instead of drifting
        // into a state nobody designed.
        assert_eq!(transition(State::Idle, &SessionInput::SpeechStarted), None);
        assert_eq!(transition(State::Idle, &SessionInput::ToolFinished), None);
        assert_eq!(
            transition(State::Connecting, &SessionInput::ToolCalled),
            None
        );
    }

    #[test]
    fn a_terminal_error_returns_to_idle_from_any_active_state() {
        for s in [
            State::Connecting,
            State::Listening,
            State::Speaking,
            State::ToolRunning,
        ] {
            assert_eq!(
                transition(s, &SessionInput::TerminalError),
                Some(State::Idle)
            );
        }
    }

    #[test]
    fn a_transient_error_returns_to_connecting_for_backoff() {
        assert_eq!(
            transition(State::Listening, &SessionInput::TransientError),
            Some(State::Connecting)
        );
        assert_eq!(
            transition(State::Speaking, &SessionInput::TransientError),
            Some(State::Connecting)
        );
    }

    #[test]
    fn engine_may_only_be_switched_while_idle() {
        // The rule that removes every hard mid-flight teardown case.
        assert!(can_switch_engine(State::Idle));
        for s in [
            State::Connecting,
            State::Listening,
            State::Thinking,
            State::ToolRunning,
            State::Speaking,
            State::Interrupting,
        ] {
            assert!(
                !can_switch_engine(s),
                "{s:?} must not permit an engine switch"
            );
        }
    }
}
