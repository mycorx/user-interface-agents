// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! An MCP bearer token sourced from an OAuth 2.1 authorization server.
//!
//! Almost all of the work belongs to rmcp: `AuthorizationManager::get_access_token`
//! already refreshes proactively inside a buffer window and persists the result
//! through the `CredentialStore`. This type's whole job is to present that as a
//! `TokenProvider` and to translate rmcp's error vocabulary into ours without
//! leaking anything.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::transport::auth::{AuthorizationManager, CredentialStore};
use uia_core::tools::ToolError;

use crate::token::TokenProvider;

pub struct OAuthProvider {
    manager: Arc<AuthorizationManager>,
    // `AuthorizationManager` has no public method to clear its own credential
    // store (only a private field and a `set_credential_store` setter that
    // takes ownership), so `invalidate` needs a direct handle to the same
    // store the manager was configured with. Callers MUST pass the same
    // `Arc<dyn CredentialStore>` (or a clone that shares the same backing
    // state) that the manager uses internally, or `invalidate` will clear the
    // wrong thing.
    credential_store: Arc<dyn CredentialStore>,
}

/// Hand-written. A derived `Debug` on a type whose entire purpose is holding
/// access to tokens is a leak waiting for someone to add a field.
impl std::fmt::Debug for OAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthProvider").finish_non_exhaustive()
    }
}

impl OAuthProvider {
    pub fn new(
        manager: Arc<AuthorizationManager>,
        credential_store: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            manager,
            credential_store,
        }
    }
}

#[async_trait]
impl TokenProvider for OAuthProvider {
    async fn token(&self) -> Result<Option<String>, ToolError> {
        match self.manager.get_access_token().await {
            Ok(token) => Ok(Some(token)),
            // Not signed in yet, the refresh token is gone, or refresh failed
            // for some other reason. All are states, not failures: `None`
            // lets the request go out bare so its 401 can be read. An `Err`
            // here would break the reactive model. Deliberately logged as a
            // static message only, never the error's own `Display`/`Debug` -
            // some `AuthError` variants nest provider response text, and this
            // breadcrumb must never become the mechanism that leaks a token.
            Err(_) => {
                tracing::debug!(
                    "OAuth token unavailable; request will proceed without a bearer token"
                );
                Ok(None)
            }
        }
    }

    async fn invalidate(&self, _challenge: Option<&str>) {
        // Drop the rejected credentials so the retry cannot reuse them. Failure
        // is ignored on purpose: the retry's own outcome is the signal, and a
        // store that will not clear is already reported by the next `token()`
        // returning `None`.
        let _ = self.credential_store.clear().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::auth::{InMemoryCredentialStore, OAuthTokenResponse, StoredCredentials};

    #[test]
    fn authorization_manager_is_send_and_sync() {
        // `TokenProvider: Send + Sync` and every call site holds the provider as
        // `Arc<dyn TokenProvider>`, so this is a requirement of the design. Assert
        // it at compile time rather than discovering it three files later.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AuthorizationManager>();
    }

    fn make_token_response(access_token: &str, expires_in_secs: u64) -> OAuthTokenResponse {
        use oauth2::{AccessToken, basic::BasicTokenType};
        use rmcp::transport::auth::VendorExtraTokenFields;

        let mut resp = OAuthTokenResponse::new(
            AccessToken::new(access_token.to_string()),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );
        resp.set_expires_in(Some(&std::time::Duration::from_secs(expires_in_secs)));
        resp
    }

    #[tokio::test]
    async fn a_provider_with_no_stored_credentials_yields_none_not_an_error() {
        // "Not signed in" is a state, not a failure. Returning Err here would make
        // the first request to a fresh server fail outright, and that request is
        // exactly the one whose 401 teaches us how to authenticate.
        //
        // Precondition checked against rmcp 3.1.4's auth.rs: `AuthorizationManager::new`
        // builds a reqwest client and delegates to `new_inner`, which only
        // constructs the struct (default in-memory credential/state stores, no
        // stored scopes) and returns - no network call. Safe to run unconditionally.
        let manager = AuthorizationManager::new("https://example.test/mcp")
            .await
            .expect("constructing a manager does no network I/O");
        let store: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());
        let provider = OAuthProvider::new(Arc::new(manager), store);
        assert_eq!(provider.token().await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_error_from_the_manager_never_quotes_a_token() {
        // The redaction rule the trait doc states, enforced. This error text
        // reaches stderr via session.rs's eprintln!.
        //
        // Drive a genuine `Err` out of `get_access_token` deterministically and
        // without network I/O: store an *expired* token, but never call
        // `configure_client` on the manager, so refreshing hits
        // `AuthError::InternalError("OAuth client not configured")` immediately
        // rather than making an HTTP request.
        let secret = "super-secret-access-token-value";
        let mut manager = AuthorizationManager::new("https://example.test/mcp")
            .await
            .unwrap();
        let store = InMemoryCredentialStore::new();
        manager.set_credential_store(store.clone());
        store
            .save(StoredCredentials::new(
                "test-client".to_string(),
                Some(make_token_response(secret, 3600)),
                vec![],
                // Received long enough ago that it reads as expired, forcing a
                // refresh attempt.
                Some(0),
            ))
            .await
            .unwrap();

        let store: Arc<dyn CredentialStore> = Arc::new(store);
        let provider = OAuthProvider::new(Arc::new(manager), store);

        // Capture tracing output for the duration of the call so the `debug!`
        // breadcrumb can be inspected without adding a logging dependency.
        let messages = capture_tracing(|| async { provider.token().await.unwrap() }).await;

        assert_eq!(
            messages.0, None,
            "an unavailable token must surface as None, not an Err"
        );
        let logged = messages.1.join("\n");
        assert!(
            !logged.contains("Bearer"),
            "logged breadcrumb must never quote the scheme+token: {logged}"
        );
        assert!(
            !logged.contains(secret),
            "logged breadcrumb must never quote the token value: {logged}"
        );
    }

    #[tokio::test]
    async fn invalidate_clears_the_stored_credentials() {
        // The reactive half of Task 1: after a 401, the next `token()` must not
        // hand back the same rejected value from the credential store.
        let mut manager = AuthorizationManager::new("https://example.test/mcp")
            .await
            .unwrap();
        let store = InMemoryCredentialStore::new();
        manager.set_credential_store(store.clone());
        store
            .save(StoredCredentials::new(
                "test-client".to_string(),
                None,
                vec![],
                None,
            ))
            .await
            .unwrap();
        assert!(
            store.load().await.unwrap().is_some(),
            "test setup: credentials must be present before invalidate"
        );

        let store_handle: Arc<dyn CredentialStore> = Arc::new(store.clone());
        let provider = OAuthProvider::new(Arc::new(manager), store_handle);

        provider.invalidate(None).await;

        assert!(
            store.load().await.unwrap().is_none(),
            "invalidate must clear the shared credential store"
        );
    }

    /// Runs `f` with a capturing `tracing::Subscriber` installed as the default
    /// for the current thread, returning `f`'s result alongside every event
    /// message recorded during the call. No `tracing-subscriber` dependency is
    /// pulled in for this: the capture is a minimal hand-rolled `Subscriber`.
    ///
    /// Relies on `#[tokio::test]`'s default single-threaded runtime so the
    /// thread-local dispatcher set by `with_default` stays in effect across
    /// `.await` points; do not call this from a `flavor = "multi_thread"` test.
    async fn capture_tracing<F, Fut, T>(f: F) -> (T, Vec<String>)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        use std::sync::Mutex;

        #[derive(Default)]
        struct Capturing {
            messages: Mutex<Vec<String>>,
        }

        struct MessageVisitor(String);
        impl tracing::field::Visit for MessageVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if !self.0.is_empty() {
                    self.0.push(' ');
                }
                self.0.push_str(&format!("{}={:?}", field.name(), value));
            }
        }

        impl tracing::Subscriber for Capturing {
            fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {
            }
            fn event(&self, event: &tracing::Event<'_>) {
                let mut visitor = MessageVisitor(String::new());
                event.record(&mut visitor);
                self.messages.lock().unwrap().push(visitor.0);
            }
            fn enter(&self, _span: &tracing::span::Id) {}
            fn exit(&self, _span: &tracing::span::Id) {}
        }

        let subscriber = Arc::new(Capturing::default());
        let dispatch = tracing::Dispatch::new(subscriber.clone());
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let result = f().await;
        let messages = subscriber.messages.lock().unwrap().clone();
        (result, messages)
    }
}
