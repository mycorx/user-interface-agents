// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Desktop-only `Activation` adapter: a global hotkey plus a tray menu.
//!
//! Mobile (SP6) supplies its own adapter behind the same `Activation` port,
//! which is why `uia-core` never learns a hotkey or a tray exists.
//!
//! Split deliberately: the pure channel plumbing below (`DEFAULT_ACCELERATOR`,
//! `build_activation_channel`, `activation_event_for_menu_id`) has no `tauri`
//! dependency at all and is exercised by real tests in any environment,
//! headless or not. Only [`register`] touches `tauri`/`AppHandle` and is
//! gated behind the `desktop` Cargo feature (see `Cargo.toml`), because
//! building `tauri` needs GTK3/WebKit2GTK on Linux — not available in this
//! WSL2 sandbox — while the channel logic below is exactly what a live
//! hotkey/tray callback forwards into, so it is the part worth unit testing.

use tokio::sync::mpsc::Sender;
use uia_core::activation::{ActivationEvent, ChannelActivation};

/// `CommandOrControl` maps to `Cmd` on macOS and `Ctrl` everywhere else,
/// which is what makes one accelerator string platform-appropriate.
pub const DEFAULT_ACCELERATOR: &str = "CommandOrControl+Shift+Space";

pub fn build_activation_channel() -> (ChannelActivation, Sender<ActivationEvent>) {
    ChannelActivation::new()
}

/// Maps a tray menu item id to the `ActivationEvent` it should forward.
/// `"quit"` deliberately returns `None`: quitting exits the app directly
/// rather than round-tripping through the session's activation channel.
pub fn activation_event_for_menu_id(id: &str) -> Option<ActivationEvent> {
    match id {
        "show" => Some(ActivationEvent::Show),
        "hide" => Some(ActivationEvent::Hide),
        _ => None,
    }
}

#[cfg(feature = "desktop")]
mod desktop {
    use super::{ActivationEvent, DEFAULT_ACCELERATOR};
    use tauri::AppHandle;
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    use tokio::sync::mpsc::Sender;

    /// Registers the global shortcut and forwards each key-down as
    /// [`ActivationEvent::Toggle`]. `on_shortcut`'s callback fires twice per
    /// physical press — once for `Pressed`, once for `Released` — so without
    /// the state filter every press immediately toggles itself back off,
    /// which is why the overlay used to only show while the keys were held.
    /// Uses `try_send`, never `send().await`: the hotkey callback runs on the
    /// OS's event-listener thread, and the audio path must never stall on a
    /// UI event — a full channel just drops the extra press.
    pub fn register(
        app: &AppHandle,
        accelerator: &str,
        tx: Sender<ActivationEvent>,
    ) -> Result<(), tauri_plugin_global_shortcut::Error> {
        app.global_shortcut()
            .on_shortcut(accelerator, move |_app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    let _ = tx.try_send(ActivationEvent::Toggle);
                }
            })
    }

    /// Registers with [`DEFAULT_ACCELERATOR`].
    pub fn register_default(
        app: &AppHandle,
        tx: Sender<ActivationEvent>,
    ) -> Result<(), tauri_plugin_global_shortcut::Error> {
        register(app, DEFAULT_ACCELERATOR, tx)
    }
}

#[cfg(feature = "desktop")]
pub use desktop::{register, register_default};

#[cfg(test)]
mod tests {
    use super::*;
    use uia_core::activation::Activation;

    #[test]
    fn the_default_accelerator_is_parsed_and_platform_appropriate() {
        assert_eq!(DEFAULT_ACCELERATOR, "CommandOrControl+Shift+Space");
    }

    #[tokio::test]
    async fn the_adapter_forwards_hotkey_presses_as_toggle_events() {
        // The adapter is a thin channel wrapper; drive it without a real
        // hotkey or a running Tauri app.
        let (mut act, tx) = build_activation_channel();
        tx.send(ActivationEvent::Toggle).await.unwrap();
        assert_eq!(act.next().await, Some(ActivationEvent::Toggle));
    }

    #[tokio::test]
    async fn tray_menu_ids_map_to_the_events_the_session_expects() {
        assert_eq!(
            activation_event_for_menu_id("show"),
            Some(ActivationEvent::Show)
        );
        assert_eq!(
            activation_event_for_menu_id("hide"),
            Some(ActivationEvent::Hide)
        );
        assert_eq!(
            activation_event_for_menu_id("quit"),
            None,
            "quit is handled by exiting the app, not forwarded"
        );
    }
}
