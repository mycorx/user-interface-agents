// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Where a request's bearer token comes from, asked freshly every time.
//!
//! The bug this exists to fix: `http_transport_config` bakes headers into
//! `StreamableHttpClientTransportConfig.custom_headers` once, at connect time, and
//! that map is then cloned into every request for the life of the transport. A
//! OAuth access token lives about an hour, so the connection dies mid-session and
//! the only cure was pasting a new token by hand. A refresh routine would have had
//! nowhere to put its result.
//!
//! This trait is that place. rmcp's `StreamableHttpClient` passes `auth_header` as a
//! per-call parameter, so a wrapper can consult a provider on every request.
//!
//! Scope note: this carries the *refreshable bearer token* only. Arbitrary static
//! headers (`X-API-Key`, `Authorization: Basic …`) stay in `custom_headers`, because
//! rmcp applies this value with reqwest's `bearer_auth` — always `Authorization`,
//! always the `Bearer` scheme. Frozen is fine for those: a long-lived PAT has
//! nothing to refresh.

use async_trait::async_trait;
use uia_core::tools::ToolError;

/// Supplies the bearer token for the next request.
///
/// Returns the **bare** token, never `"Bearer …"` — rmcp adds the scheme via
/// reqwest's `bearer_auth`, so a prefixed value would be sent as
/// `Authorization: Bearer Bearer <token>`.
///
/// # Error text must never carry credentials
///
/// An `Err` returned here is not swallowed: `AuthedHttpClient` interpolates its
/// `Display` into a transport error, which surfaces as `ToolError::Transport(..)`,
/// which callers (e.g. `uia-app`'s session loop) print with `eprintln!`. That means
/// this error's text reaches a terminal. An implementation — an OAuth
/// provider in particular — MUST NOT let a token, refresh token, or authorization
/// code appear anywhere in the error it returns (directly, via a wrapped SDK error's
/// `Display`, or via `Debug`-formatting a response). Redact or summarize instead.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    /// `Ok(None)` means the provider declines to supply a token for this request;
    /// the caller's existing `auth_header`, if any, stands unchanged.
    ///
    /// Deliberately not an error: the first request to a server whose auth is not
    /// yet established must go out bare (or with whatever header the caller already
    /// had), so the server's 401 challenge can be read and answered. Missing
    /// credentials are a state, not a failure. See `AuthedHttpClient::resolve_auth_header`,
    /// which implements this as `token.or(auth_header)`.
    async fn token(&self) -> Result<Option<String>, ToolError>;

    /// Called when the server rejected the token this provider supplied, with
    /// the `WWW-Authenticate` challenge from the 401 if the server sent one.
    ///
    /// The contract is "forget what you cached"; the caller retries once
    /// afterwards. Deliberately infallible - a provider that cannot invalidate
    /// has nothing useful to report, and the retry's own failure is the real
    /// signal. Deliberately defaulted to a no-op so a provider with nothing to
    /// forget (`NoToken`, a static token) needs no code.
    ///
    /// The same redaction rule as `token` applies to anything logged here.
    async fn invalidate(&self, _challenge: Option<&str>) {}
}

/// Never supplies a token. The behaviour of every server configured today.
///
/// Servers using an arbitrary static header still work exactly as before: their
/// header travels in `custom_headers`, untouched by this path.
pub struct NoToken;

#[async_trait]
impl TokenProvider for NoToken {
    async fn token(&self) -> Result<Option<String>, ToolError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_token_yields_none_so_the_request_goes_out_unauthenticated() {
        // `None` is not an error. It is what drives the reactive model: the first
        // request to an unknown server goes out bare precisely so its 401 challenge
        // can be read and answered. A provider that errored instead would turn
        // "not signed in yet" into "broken".
        assert_eq!(NoToken.token().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_provider_can_yield_a_bare_token() {
        struct Fixed(&'static str);

        #[async_trait::async_trait]
        impl TokenProvider for Fixed {
            async fn token(&self) -> Result<Option<String>, ToolError> {
                Ok(Some(self.0.to_string()))
            }
        }

        // The BARE token, with no "Bearer " prefix: rmcp applies the scheme itself
        // via reqwest's `bearer_auth`. Returning "Bearer x" here would put
        // "Authorization: Bearer Bearer x" on the wire.
        assert_eq!(
            Fixed("abc123").token().await.unwrap().as_deref(),
            Some("abc123")
        );
    }

    #[tokio::test]
    async fn a_provider_is_usable_behind_a_trait_object() {
        // Every call site holds one as `Arc<dyn TokenProvider>`, so object safety is
        // a requirement of the design, not an accident of it.
        let p: std::sync::Arc<dyn TokenProvider> = std::sync::Arc::new(NoToken);
        assert_eq!(p.token().await.unwrap(), None);
    }
}
