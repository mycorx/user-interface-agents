// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The running app's view of each server's alert settings, and the small
//! mappings from window and engine facts to what [`super::route`] reads.
//!
//! Settings are written through: `set_mcp_alerts` saves the registry and then
//! [`LiveAlertSettings::apply`]s the stored value here, so `show` and
//! `wake_agent` take effect on the next alert with no reconnect. The listener
//! reads `alert_resource` only when it discovers feeds, so a changed override
//! takes effect on the next connect.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use uia_core::engine::TextTurnSupport;
use uia_mcp::alert_listener::{AlertSettingsView, SharedAlertSettings};

use super::{TextTurns, UiState};
use crate::mcp_registry::{AlertSettings, McpRegistry};

struct LiveEntry {
    /// Shared with that server's listener.
    view: SharedAlertSettings,
    wake_agent: bool,
}

impl LiveEntry {
    fn new(settings: &AlertSettings) -> Self {
        Self {
            view: Arc::new(RwLock::new(view_of(settings))),
            wake_agent: settings.wake_agent,
        }
    }
}

fn view_of(settings: &AlertSettings) -> AlertSettingsView {
    AlertSettingsView {
        show: settings.show,
        alert_resource: settings.alert_resource.clone(),
    }
}

/// Every server's alert settings, by server name.
#[derive(Default)]
pub struct LiveAlertSettings {
    entries: Mutex<BTreeMap<String, LiveEntry>>,
}

impl LiveAlertSettings {
    /// One entry for every local and remote server in `registry`.
    pub fn from_registry(registry: &McpRegistry) -> Self {
        let entries = registry
            .local_servers
            .iter()
            .map(|l| (&l.name, &l.alerts))
            .chain(registry.remote_servers.iter().map(|r| (&r.name, &r.alerts)))
            .map(|(name, alerts)| (name.clone(), LiveEntry::new(alerts)))
            .collect();
        Self {
            entries: Mutex::new(entries),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, LiveEntry>> {
        // A panic elsewhere must not silence alerts for the rest of the run.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The view `server`'s listener reads. An unknown server gets the
    /// defaults, kept so a later [`apply`](Self::apply) still reaches it.
    pub fn view_for(&self, server: &str) -> SharedAlertSettings {
        let mut entries = self.entries();
        let entry = entries
            .entry(server.to_string())
            .or_insert_with(|| LiveEntry::new(&AlertSettings::default()));
        Arc::clone(&entry.view)
    }

    /// Make `settings` live for `server`, in the same view its listener holds.
    pub fn apply(&self, server: &str, settings: &AlertSettings) {
        let mut entries = self.entries();
        match entries.get_mut(server) {
            Some(entry) => {
                *entry.view.write().unwrap_or_else(|e| e.into_inner()) = view_of(settings);
                entry.wake_agent = settings.wake_agent;
            }
            None => {
                entries.insert(server.to_string(), LiveEntry::new(settings));
            }
        }
    }

    /// Whether `server`'s alerts are shown at all. Unknown servers use the
    /// default (shown).
    pub fn shows(&self, server: &str) -> bool {
        self.entries()
            .get(server)
            .is_none_or(|entry| entry.view.read().unwrap_or_else(|e| e.into_inner()).show)
    }

    /// Whether the agent may speak `server`'s alerts: only while they are
    /// shown. Unknown servers use the default (no).
    pub fn wakes_agent(&self, server: &str) -> bool {
        self.shows(server) && self.entries().get(server).is_some_and(|e| e.wake_agent)
    }
}

/// What the router may assume about typed turns. `Unknown` counts as
/// supported only once a typed turn has gone through this run; an engine
/// that later refuses one reports `Unsupported`, which wins.
pub fn text_turns_from(support: &TextTurnSupport, typed_turn_succeeded: bool) -> TextTurns {
    match support {
        TextTurnSupport::Supported => TextTurns::Supported,
        TextTurnSupport::Unsupported(_) => TextTurns::Unsupported,
        TextTurnSupport::Unknown if typed_turn_succeeded => TextTurns::Supported,
        TextTurnSupport::Unknown => TextTurns::Unsupported,
    }
}

/// The main window as the user sees it, from its `is_visible` and
/// `is_minimized` answers.
pub fn ui_state_from(visible: bool, minimised: bool) -> UiState {
    if !visible {
        UiState::Hidden
    } else if minimised {
        UiState::Minimised
    } else {
        UiState::Visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_registry::{LocalServerEntry, RemoteEntry};

    fn settings(show: bool, wake_agent: bool, alert_resource: Option<&str>) -> AlertSettings {
        AlertSettings {
            show,
            wake_agent,
            alert_resource: alert_resource.map(Into::into),
        }
    }

    fn registry() -> McpRegistry {
        let mut registry = McpRegistry {
            local_servers: vec![LocalServerEntry {
                name: "clock".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        registry.remote_servers.push(RemoteEntry {
            name: "calendar".into(),
            ..Default::default()
        });
        registry.set_alerts(
            "calendar",
            settings(true, true, Some("mcp://calendar/feed")),
        );
        registry
    }

    #[test]
    fn every_registered_server_starts_with_its_stored_settings() {
        let live = LiveAlertSettings::from_registry(&registry());
        assert!(live.shows("clock"));
        assert!(!live.wakes_agent("clock"));
        assert!(live.wakes_agent("calendar"));
        let view = live.view_for("calendar");
        let view = view.read().unwrap();
        assert!(view.show);
        assert_eq!(view.alert_resource.as_deref(), Some("mcp://calendar/feed"));
    }

    #[test]
    fn an_applied_change_reaches_the_view_the_listener_already_holds() {
        let live = LiveAlertSettings::from_registry(&registry());
        let held = live.view_for("clock");
        live.apply("clock", &settings(false, false, Some("mcp://clock/feed")));
        let view = held.read().unwrap();
        assert!(!view.show);
        assert_eq!(view.alert_resource.as_deref(), Some("mcp://clock/feed"));
        drop(view);
        assert!(!live.shows("clock"));
    }

    #[test]
    fn wake_up_applies_on_the_next_alert_without_a_reconnect() {
        let live = LiveAlertSettings::from_registry(&registry());
        assert!(!live.wakes_agent("clock"));
        live.apply("clock", &settings(true, true, None));
        assert!(live.wakes_agent("clock"));
    }

    #[test]
    fn a_hidden_server_never_wakes_the_agent_even_if_wake_up_is_on() {
        let live = LiveAlertSettings::from_registry(&registry());
        live.apply("calendar", &settings(false, true, None));
        assert!(!live.shows("calendar"));
        assert!(!live.wakes_agent("calendar"));
    }

    #[test]
    fn an_unknown_server_uses_the_defaults_and_can_still_be_changed_later() {
        let live = LiveAlertSettings::default();
        assert!(live.shows("ghost"));
        assert!(!live.wakes_agent("ghost"));
        let held = live.view_for("ghost");
        live.apply("ghost", &settings(false, false, None));
        assert!(!held.read().unwrap().show);
    }

    #[test]
    fn text_turns_are_supported_or_unsupported_as_the_engine_says() {
        for seen in [false, true] {
            assert_eq!(
                text_turns_from(&TextTurnSupport::Supported, seen),
                TextTurns::Supported
            );
            assert_eq!(
                text_turns_from(&TextTurnSupport::Unsupported("no".into()), seen),
                TextTurns::Unsupported
            );
        }
    }

    #[test]
    fn unknown_text_turn_support_counts_only_after_a_typed_turn_went_through() {
        assert_eq!(
            text_turns_from(&TextTurnSupport::Unknown, false),
            TextTurns::Unsupported
        );
        assert_eq!(
            text_turns_from(&TextTurnSupport::Unknown, true),
            TextTurns::Supported
        );
    }

    #[test]
    fn a_window_that_is_not_visible_is_hidden_whether_or_not_it_is_minimised() {
        assert_eq!(ui_state_from(false, false), UiState::Hidden);
        assert_eq!(ui_state_from(false, true), UiState::Hidden);
    }

    #[test]
    fn a_visible_window_is_minimised_or_visible() {
        assert_eq!(ui_state_from(true, true), UiState::Minimised);
        assert_eq!(ui_state_from(true, false), UiState::Visible);
    }
}
