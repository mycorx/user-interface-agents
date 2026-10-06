// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

pub mod activation_desktop;
pub mod config;
pub mod hud;
pub mod mcp_registry;
pub mod memory;
pub mod oauth_flow;
pub mod oauth_store;
pub mod paths;
pub mod persona_executor;
pub mod personas;
pub mod secrets;
pub mod session;
pub mod settings;
pub mod time;
pub mod time_executor;
pub mod wsl;

/// The IPC payload contract: the HUD only ever renders the string Rust
/// emits on `uia://state`, so this mapping is the thing worth testing —
/// no business logic crosses into the WebView.
pub fn state_label(s: uia_core::session::State) -> &'static str {
    use uia_core::session::State as S;
    match s {
        S::Idle => "Idle",
        S::Connecting => "Connecting",
        S::Listening => "Listening",
        S::Thinking => "Thinking",
        S::ToolRunning => "ToolRunning",
        S::Speaking => "Speaking",
        S::Interrupting => "Interrupting",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uia_core::session::State;

    #[test]
    fn state_serialises_to_the_name_the_frontend_switches_on() {
        assert_eq!(state_label(State::Listening), "Listening");
        assert_eq!(state_label(State::ToolRunning), "ToolRunning");
        assert_eq!(state_label(State::Idle), "Idle");
    }

    #[test]
    fn every_state_variant_has_a_label() {
        // A guard against a variant silently falling through to a default —
        // there is no default arm above, so this would be a compile error,
        // but the test documents the intent explicitly.
        for (s, label) in [
            (State::Idle, "Idle"),
            (State::Connecting, "Connecting"),
            (State::Listening, "Listening"),
            (State::Thinking, "Thinking"),
            (State::ToolRunning, "ToolRunning"),
            (State::Speaking, "Speaking"),
            (State::Interrupting, "Interrupting"),
        ] {
            assert_eq!(state_label(s), label);
        }
    }
}
