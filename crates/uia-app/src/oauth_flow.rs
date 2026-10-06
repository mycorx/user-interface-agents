// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Deep-link glue. Deliberately almost empty.
//!
//! Everything with a decision in it lives in `uia_mcp::callback`, which is pure
//! and unit-tested. This file is the part that cannot be tested in CI — it
//! touches the OS — so the rule is that it contains nothing worth testing.
//!
//! Not verifiable in WSL2: `xdg-mime`, `update-desktop-database` and `xdg-open`
//! are all absent and there is no `XDG_CURRENT_DESKTOP`. The end-to-end proof is
//! the manual procedure in `docs/MANUAL-TEST.md`, on a real desktop.
//!
//! Only [`handle_deep_link`] itself needs `tauri::AppHandle`, and only it is
//! `#[cfg(feature = "desktop")]`. Everything else here -- the state-routing
//! decision this module exists for -- is plain `url`/`tokio`, so it compiles
//! and is unit-tested the same way with `desktop` on or off, same as
//! `config`/`settings`/`paths`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use uia_mcp::callback::{CallbackError, CallbackListener, PendingAuthorizations};

pub fn is_callback_url(arg: &str) -> bool {
    arg.starts_with("uia://")
}

/// Delivers a browser redirect to whichever pending [`uia_mcp::callback`] flow
/// is waiting on it, matched by the CSRF `state` that flow started with.
///
/// One `oneshot` sender per pending `state`, not a broadcast channel every
/// waiter has to filter: each call to [`Self::await_callback`] cares about
/// exactly one state, and a redirect belongs to exactly one flow. The map
/// itself mirrors `PendingAuthorizations::by_state`'s own
/// `Arc<Mutex<HashMap<...>>>` shape for the same reason it works there — this
/// is a synchronous lookup guarded by a lock, not something that needs the
/// rest of tokio's sync toolkit.
///
/// `#[derive(Clone)]`, not `Arc<DeepLinkListener>`: it is registered with
/// `.manage()` in `main.rs` and reached back via `app.state::<Self>()`, so
/// nothing outside this module needs to hold a clone at all — but a listener
/// implementation being cheaply `Clone` (not just usable through `&self`,
/// which the `CallbackListener` trait already only requires) costs nothing
/// and matches `PendingAuthorizations`'s own shape.
#[derive(Default, Clone)]
pub struct DeepLinkListener {
    waiters: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    /// The routing table `mcp_registry::preview_remote` registers each
    /// pending sign-in into (via [`Self::pending_authorizations`]'s clone).
    /// `deliver` consults it first, before falling back to its own raw-URL
    /// parse — see `deliver`'s doc comment for why both still exist.
    routes: PendingAuthorizations,
}

#[async_trait::async_trait]
impl CallbackListener for DeepLinkListener {
    async fn await_callback(&self, expected_state: &str) -> Result<String, CallbackError> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .expect("DeepLinkListener mutex poisoned")
            .insert(expected_state.to_string(), tx);

        // The sender is dropped, without ever being called, exactly when
        // nothing else holds it -- i.e. this state was never delivered and
        // nobody else is going to. `Cancelled` covers both a stale flow that
        // timed out elsewhere and one this process is shutting down; neither
        // is a matter this file has enough information to tell apart.
        rx.await.map_err(|_| CallbackError::Cancelled)
    }
}

impl DeepLinkListener {
    /// The routing table `mcp_registry::preview_remote` registers each
    /// pending sign-in's `(state, server_name)` pair into, via
    /// `uia_mcp::callback::PendingAuthorizations::register`. Cloning it out
    /// (rather than exposing `&self.routes`) lets the two independent
    /// managed-state values in `main.rs` — this listener and the shared
    /// `PendingAuthorizations` `preview_remote` writes into — stay backed by
    /// the exact same `Arc<Mutex<..>>`, `PendingAuthorizations` being cheap
    /// `Clone` for the same reason `DeepLinkListener` itself is.
    pub fn pending_authorizations(&self) -> PendingAuthorizations {
        self.routes.clone()
    }

    /// Matches `url` against a pending state and, if one exists, wakes it.
    ///
    /// `#[cfg(feature = "desktop")]`, like [`handle_deep_link`] (its only
    /// caller): nothing about this method itself needs `tauri`, but it exists
    /// solely to serve that OS-facing entry point, so it stays gated the same
    /// way rather than being "never used" dead code in a non-desktop build.
    ///
    /// Routes through `uia_mcp::callback::PendingAuthorizations::route`
    /// first — gaining rmcp's own validation (a genuine registration for the
    /// state, not just a state that merely parses) and, incidentally,
    /// `server_name`/`issuer`, though this method still only needs `state`
    /// to find the right waiter. This is NOT an `rmcp` dependency on
    /// `uia-app`'s part: `route` is a plain function `uia_mcp::callback`
    /// exposes that returns a plain, `rmcp`-free `Routed` struct — an
    /// earlier version of this comment claimed wiring it in here would
    /// require depending on `rmcp` directly, which was never true (it was
    /// only ever a plumbing gap: nothing had threaded a shared
    /// `PendingAuthorizations` between `preview_remote` and this listener
    /// yet). `uia-app` still has no *direct* `rmcp` dependency in
    /// `Cargo.toml`; everything `rmcp`-shaped stays behind the `uia-mcp`
    /// boundary exactly as the plan requires.
    ///
    /// `route` requires `code` to be present to parse at all, so a bare
    /// denial redirect (e.g. `?error=access_denied&state=X`, no `code`)
    /// fails it with `Malformed`. Falling straight through to "dropped
    /// silently" there would resurrect a real bug this module used to
    /// describe as fixed: without ever waking the waiter, that sign-in would
    /// sit until `main.rs`'s `OAUTH_BROWSER_TIMEOUT` (five minutes) reaps it,
    /// instead of failing right away with a clear denial. So `Malformed`
    /// falls back to this method's own raw-URL `state` extraction — the
    /// same parse this method used exclusively before this integration —
    /// which has no `code`-must-be-present requirement and still finds the
    /// waiter to wake (with the denial URL itself, for `handle_callback_url`
    /// to report as a real failure rather than a silent timeout).
    /// `UnknownState` (a forged, stale, or already-consumed state) and
    /// `Cancelled` are dropped silently either way — there is nothing a
    /// fallback parse could add for either.
    #[cfg(feature = "desktop")]
    fn deliver(&self, url: &str) {
        match self.routes.route(url) {
            Ok(routed) => {
                self.wake(&routed.state, url);
                return;
            }
            Err(CallbackError::UnknownState) | Err(CallbackError::Cancelled) => return,
            Err(CallbackError::Malformed(_)) => {
                // Fall through to the raw-URL fallback below.
            }
        }

        let Some(state) = url::Url::parse(url).ok().and_then(|parsed| {
            parsed
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned())
        }) else {
            return;
        };
        self.wake(&state, url);
    }

    /// Delivers `url` to the waiter registered for `state`, if any. A URL
    /// that matches no pending state — malformed, forged, stale, or a
    /// replay of an already-consumed one — is dropped silently. This is the
    /// process's own front door for untrusted OS input (argv, or another
    /// app's `on_open_url`), not a channel with a caller to report back to.
    #[cfg(feature = "desktop")]
    fn wake(&self, state: &str, url: &str) {
        if let Some(tx) = self
            .waiters
            .lock()
            .expect("DeepLinkListener mutex poisoned")
            .remove(state)
        {
            let _ = tx.send(url.to_string());
        }
    }
}

/// Routes one incoming `uia://` redirect to the app's managed
/// [`DeepLinkListener`]. Called from both delivery paths `main.rs` wires up:
/// the `single-instance` argv callback (Linux/Windows) and the deep-link
/// plugin's `on_open_url` (macOS).
#[cfg(feature = "desktop")]
pub fn handle_deep_link(app: &tauri::AppHandle, url: &str) {
    use tauri::Manager;
    app.state::<DeepLinkListener>().deliver(url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_uia_callback_url_is_forwarded_to_the_router() {
        // argv can carry anything. A stray argument that merely looks URL-ish must
        // not be handed to the router, and must not panic.
        assert!(is_callback_url("uia://callback?code=x&state=y"));
        assert!(!is_callback_url("https://example.test/"));
        assert!(!is_callback_url("--some-flag"));
        assert!(!is_callback_url(""));
    }
}
