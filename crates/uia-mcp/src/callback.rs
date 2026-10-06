// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Which pending authorization does an incoming callback belong to?
//!
//! Deliberately thin. rmcp already provides the URL parser
//! (`AuthorizationCallback::from_redirect_url`) and validates the CSRF `state`
//! against its own state store during the token exchange. Re-implementing
//! either would be duplication with two places to get wrong.
//!
//! What is genuinely ours is routing: the app can have several servers mid-flow,
//! and a single `uia://callback` arrives with nothing but a `state` to say which
//! one it answers. That is a map lookup — pure, synchronous, and testable
//! anywhere, which is exactly why it lives here and not in the Tauri glue.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use rmcp::transport::auth::AuthorizationCallback;

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    /// Never names the state or the code: an attacker's forged callback should
    /// not have its contents echoed into a log the user might paste somewhere.
    #[error("this authorization callback does not match any pending sign-in")]
    UnknownState,
    #[error("the authorization callback was malformed: {0}")]
    Malformed(String),
    #[error("the authorization was cancelled or timed out")]
    Cancelled,
}

pub struct Routed {
    pub server_name: String,
    pub code: String,
    pub state: String,
    pub issuer: Option<String>,
}

/// Hand-written: `code` is a single-use credential.
impl std::fmt::Debug for Routed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Routed")
            .field("server_name", &self.server_name)
            .field("code", &"<redacted>")
            .field("state", &"<redacted>")
            .field("issuer", &self.issuer)
            .finish()
    }
}

#[derive(Default, Clone)]
pub struct PendingAuthorizations {
    by_state: Arc<Mutex<HashMap<String, String>>>,
}

impl PendingAuthorizations {
    pub fn register(&self, state: &str, server_name: &str) {
        self.by_state
            .lock()
            .expect("PendingAuthorizations mutex poisoned")
            .insert(state.to_string(), server_name.to_string());
    }

    /// Consumes the registration: a callback is single-use, and a replay must
    /// find nothing.
    pub fn route(&self, url: &str) -> Result<Routed, CallbackError> {
        let parsed = AuthorizationCallback::from_redirect_url(url)
            // rmcp's error names which field was missing, never a value.
            .map_err(|e| CallbackError::Malformed(e.to_string()))?;

        let server_name = self
            .by_state
            .lock()
            .expect("PendingAuthorizations mutex poisoned")
            .remove(&parsed.csrf_token)
            .ok_or(CallbackError::UnknownState)?;

        Ok(Routed {
            server_name,
            code: parsed.code,
            state: parsed.csrf_token,
            issuer: parsed.issuer,
        })
    }

    pub fn cancel(&self, state: &str) {
        self.by_state
            .lock()
            .expect("PendingAuthorizations mutex poisoned")
            .remove(state);
    }
}

/// How the authorization code gets back from the browser.
///
/// One trait, so the custom-scheme deep link and a future loopback listener are
/// two implementations rather than two code paths. Cognito permits several
/// callback URLs on one app client, so adding loopback later is a CLI call plus
/// an impl — no change here.
#[async_trait::async_trait]
pub trait CallbackListener: Send + Sync {
    /// Resolves with the full redirect URL once one arrives bearing
    /// `expected_state`, or fails if the flow is cancelled or times out.
    async fn await_callback(&self, expected_state: &str) -> Result<String, CallbackError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_routes_to_the_server_that_started_it() {
        let pending = PendingAuthorizations::default();
        pending.register("state-abc", "example-server");
        pending.register("state-xyz", "other");

        let routed = pending
            .route("uia://callback?code=CODE1&state=state-xyz")
            .expect("a registered state must route");

        assert_eq!(routed.server_name, "other");
        assert_eq!(routed.code, "CODE1");
    }

    #[test]
    fn an_unknown_state_is_refused_rather_than_guessed() {
        // This is the CSRF check at our layer: a callback whose state we never
        // issued is an attacker's, or a stale tab's. Picking "the only pending
        // one" would defeat the purpose of the state parameter entirely.
        let pending = PendingAuthorizations::default();
        pending.register("state-abc", "example-server");

        let err = pending
            .route("uia://callback?code=CODE1&state=forged")
            .expect_err("an unissued state must not route anywhere");
        assert!(matches!(err, CallbackError::UnknownState));
    }

    #[test]
    fn a_state_is_consumed_so_a_replayed_callback_is_refused() {
        let pending = PendingAuthorizations::default();
        pending.register("state-abc", "example-server");
        pending
            .route("uia://callback?code=C&state=state-abc")
            .unwrap();

        let err = pending
            .route("uia://callback?code=C&state=state-abc")
            .expect_err("replaying a consumed callback must fail");
        assert!(matches!(err, CallbackError::UnknownState));
    }

    #[test]
    fn a_url_with_no_code_is_refused_with_a_clear_error() {
        // The user hit "cancel" on the consent screen, or the AS sent ?error=.
        let pending = PendingAuthorizations::default();
        pending.register("state-abc", "example-server");
        let err = pending
            .route("uia://callback?state=state-abc&error=access_denied")
            .expect_err("a callback carrying no code cannot be exchanged");
        assert!(matches!(err, CallbackError::Malformed(_)));
    }

    #[test]
    fn an_error_never_quotes_the_authorization_code() {
        // A code is a single-use credential. It must not reach a log line.
        let pending = PendingAuthorizations::default();
        let err = pending
            .route("uia://callback?code=SECRETCODE&state=nope")
            .unwrap_err();
        assert!(!format!("{err}").contains("SECRETCODE"));
        assert!(!format!("{err:?}").contains("SECRETCODE"));
    }

    #[test]
    fn a_cancelled_authorization_stops_accepting_its_callback() {
        // The browser flow timed out and the UI moved on; a late callback must
        // not resurrect it.
        let pending = PendingAuthorizations::default();
        pending.register("state-abc", "example-server");
        pending.cancel("state-abc");

        let err = pending
            .route("uia://callback?code=CODE1&state=state-abc")
            .expect_err("a cancelled authorization must not accept its callback");
        assert!(matches!(err, CallbackError::UnknownState));
    }
}
