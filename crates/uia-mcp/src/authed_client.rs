// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Consults a `TokenProvider` on every request instead of reusing a header map
//! captured at connect time.
//!
//! `StreamableHttpClient` hands `auth_header` to each method as a parameter, which
//! is what makes per-request substitution possible at all. The transport's own
//! `config.auth_header` and `config.custom_headers` are both frozen at construction,
//! so neither could ever carry a refreshed token.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use http::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};

use crate::token::TokenProvider;

/// Wraps any `StreamableHttpClient`, replacing the per-call `auth_header` with a
/// freshly-fetched token.
#[derive(Clone)]
pub struct AuthedHttpClient<C> {
    inner: C,
    // `Arc` so `Clone` is cheap and shares one provider - rmcp clones the client
    // freely, and each clone must see the same token state.
    provider: Arc<dyn TokenProvider>,
}

impl<C> AuthedHttpClient<C> {
    pub fn new(inner: C, provider: Arc<dyn TokenProvider>) -> Self {
        Self { inner, provider }
    }
}

impl<C: StreamableHttpClient> AuthedHttpClient<C> {
    /// Resolve the token to send, falling back to the caller's `auth_header` when
    /// the provider has none to offer (`Ok(None)`: the provider declines to
    /// override, so the caller's existing header stands, per `TokenProvider`'s
    /// contract).
    ///
    /// Takes the provider by owned `Arc` rather than `&self`. `StreamableHttpClient`
    /// only requires `Clone + Send + 'static` (no `Sync`), and `async fn` in a trait
    /// impl (RPITIT) captures the `&self` lifetime in its hidden future type by
    /// default regardless of whether the body still touches `self` after the first
    /// `.await` - so each trait method below is written as a plain `fn` that clones
    /// `self.inner`/`self.provider` into owned locals *before* building an
    /// `async move` block, and that block (and this helper) never sees `&self` at
    /// all. That is what keeps the returned future `Send` without requiring
    /// `C: Sync`, a bound the trait itself does not demand and that would reject an
    /// inner client that is `Send` but not `Sync`.
    ///
    /// A provider error must surface, not be swallowed. There is no clean variant
    /// for it: `type Error = C::Error` is the inner client's error type (`reqwest::Error`
    /// in production), which we cannot construct, and `StreamableHttpError`'s
    /// remaining variants are all transport-shaped. `UnexpectedServerResponse` is a
    /// poor fit by name - chosen deliberately anyway, because its `Cow<'static, str>`
    /// payload is what a human debugging this will actually read.
    async fn resolve_auth_header(
        provider: Arc<dyn TokenProvider>,
        auth_header: Option<String>,
    ) -> Result<Option<String>, StreamableHttpError<C::Error>> {
        let token = provider.token().await.map_err(|e| {
            StreamableHttpError::UnexpectedServerResponse(Cow::Owned(format!(
                "token provider failed: {e}"
            )))
        })?;
        Ok(token.or(auth_header))
    }
}

impl<C: StreamableHttpClient> StreamableHttpClient for AuthedHttpClient<C> {
    type Error = C::Error;

    // These five are written as plain `fn`s returning `async move` blocks, not as
    // `async fn`s, and each clones `self.inner`/`self.provider` into owned locals
    // *before* building that block. See `resolve_auth_header`'s doc comment for
    // why: an `async fn` trait method captures `&self` in its hidden future type
    // regardless of whether the body uses `self` after the first `.await`, which
    // would force `Self: Sync` (and so `C: Sync`) for no reason this trait
    // requires. An `async move` block built from owned locals has no such
    // dependency on `self` at all.

    fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>>>
    + Send
    + '_ {
        let inner = self.inner.clone();
        let provider = self.provider.clone();
        async move {
            let auth = Self::resolve_auth_header(provider.clone(), auth_header.clone()).await?;
            // One retry, never a loop. rmcp's transport already retries
            // transport-level failures with its own backoff; this handles
            // exactly one case that backoff cannot - a token the server
            // rejected - and a second 401 means re-authorization is needed,
            // which is a decision for the user, not this layer.
            //
            // `message`, `session_id`, and `custom_headers` are cloned to keep a
            // copy for the retry; `ClientJsonRpcMessage` is `Clone`, so this is
            // not a workaround.
            let first = inner
                .post_message(
                    uri.clone(),
                    message.clone(),
                    session_id.clone(),
                    auth,
                    custom_headers.clone(),
                )
                .await;
            match first {
                Err(StreamableHttpError::AuthRequired(ref e)) => {
                    provider.invalidate(Some(&e.www_authenticate_header)).await;
                    let auth = Self::resolve_auth_header(provider, auth_header).await?;
                    inner
                        .post_message(uri, message, session_id, auth, custom_headers)
                        .await
                }
                other => other,
            }
        }
    }

    fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> impl Future<Output = Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>>>
    + Send
    + '_ {
        let inner = self.inner.clone();
        let provider = self.provider.clone();
        async move {
            let auth = Self::resolve_auth_header(provider.clone(), auth_header.clone()).await?;
            // See `post_message` above for why this retries exactly once.
            let first = inner
                .post_message_with_max_sse_event_size(
                    uri.clone(),
                    message.clone(),
                    session_id.clone(),
                    auth,
                    custom_headers.clone(),
                    max_sse_event_size,
                )
                .await;
            match first {
                Err(StreamableHttpError::AuthRequired(ref e)) => {
                    provider.invalidate(Some(&e.www_authenticate_header)).await;
                    let auth = Self::resolve_auth_header(provider, auth_header).await?;
                    inner
                        .post_message_with_max_sse_event_size(
                            uri,
                            message,
                            session_id,
                            auth,
                            custom_headers,
                            max_sse_event_size,
                        )
                        .await
                }
                other => other,
            }
        }
    }

    fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<Output = Result<(), StreamableHttpError<Self::Error>>> + Send + '_ {
        let inner = self.inner.clone();
        let provider = self.provider.clone();
        // Deliberately NOT retried on a 401, unlike the other four methods. This
        // is teardown: its whole point is to end the session, and resurrecting
        // one by invalidating + re-authorizing + retrying on the way out would
        // work against the caller's intent. A rejected delete is simply reported.
        async move {
            let auth_header = Self::resolve_auth_header(provider, auth_header).await?;
            inner
                .delete_session(uri, session_id, auth_header, custom_headers)
                .await
        }
    }

    fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> impl Future<
        Output = Result<
            futures::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
            StreamableHttpError<Self::Error>,
        >,
    > + Send
    + '_ {
        let inner = self.inner.clone();
        let provider = self.provider.clone();
        async move {
            let auth = Self::resolve_auth_header(provider.clone(), auth_header.clone()).await?;
            // See `post_message` above for why this retries exactly once.
            let first = inner
                .get_stream(
                    uri.clone(),
                    session_id.clone(),
                    last_event_id.clone(),
                    auth,
                    custom_headers.clone(),
                )
                .await;
            match first {
                Err(StreamableHttpError::AuthRequired(ref e)) => {
                    provider.invalidate(Some(&e.www_authenticate_header)).await;
                    let auth = Self::resolve_auth_header(provider, auth_header).await?;
                    inner
                        .get_stream(uri, session_id, last_event_id, auth, custom_headers)
                        .await
                }
                other => other,
            }
        }
    }

    fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> impl Future<
        Output = Result<
            futures::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
            StreamableHttpError<Self::Error>,
        >,
    > + Send
    + '_ {
        let inner = self.inner.clone();
        let provider = self.provider.clone();
        async move {
            let auth = Self::resolve_auth_header(provider.clone(), auth_header.clone()).await?;
            // See `post_message` above for why this retries exactly once.
            let first = inner
                .get_stream_with_max_sse_event_size(
                    uri.clone(),
                    session_id.clone(),
                    last_event_id.clone(),
                    auth,
                    custom_headers.clone(),
                    max_sse_event_size,
                )
                .await;
            match first {
                Err(StreamableHttpError::AuthRequired(ref e)) => {
                    provider.invalidate(Some(&e.www_authenticate_header)).await;
                    let auth = Self::resolve_auth_header(provider, auth_header).await?;
                    inner
                        .get_stream_with_max_sse_event_size(
                            uri,
                            session_id,
                            last_event_id,
                            auth,
                            custom_headers,
                            max_sse_event_size,
                        )
                        .await
                }
                other => other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::NoToken;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use uia_core::tools::ToolError;

    /// Records the auth_header seen on each call, and which method it arrived
    /// through. `Clone` shares the log, because the trait requires `Clone` and
    /// rmcp clones the client freely.
    ///
    /// All five trait methods are overridden explicitly here (none inherit the
    /// trait's default body for the `_with_max_sse_event_size` pair) precisely so
    /// each records its own name into `calls`. If `AuthedHttpClient` ever forwarded
    /// `post_message_with_max_sse_event_size` to the plain `post_message` on
    /// `inner` instead of its exact counterpart, the default-delegating fake would
    /// mask that: the call would still land somewhere and every assertion on
    /// `seen` alone would still pass. Recording the method name makes a wrong
    /// forward visible.
    #[derive(Clone, Default)]
    struct SpyClient {
        seen: Arc<Mutex<Vec<Option<String>>>>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("spy")]
    struct SpyError;

    impl StreamableHttpClient for SpyClient {
        type Error = SpyError;

        async fn post_message(
            &self,
            _uri: Arc<str>,
            _message: rmcp::model::ClientJsonRpcMessage,
            _session_id: Option<Arc<str>>,
            auth_header: Option<String>,
            _custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
        ) -> Result<
            rmcp::transport::streamable_http_client::StreamableHttpPostResponse,
            StreamableHttpError<Self::Error>,
        > {
            self.calls.lock().unwrap().push("post_message");
            self.seen.lock().unwrap().push(auth_header);
            Err(StreamableHttpError::Client(SpyError))
        }

        async fn post_message_with_max_sse_event_size(
            &self,
            _uri: Arc<str>,
            _message: rmcp::model::ClientJsonRpcMessage,
            _session_id: Option<Arc<str>>,
            auth_header: Option<String>,
            _custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
            _max_sse_event_size: usize,
        ) -> Result<
            rmcp::transport::streamable_http_client::StreamableHttpPostResponse,
            StreamableHttpError<Self::Error>,
        > {
            self.calls
                .lock()
                .unwrap()
                .push("post_message_with_max_sse_event_size");
            self.seen.lock().unwrap().push(auth_header);
            Err(StreamableHttpError::Client(SpyError))
        }

        async fn delete_session(
            &self,
            _uri: Arc<str>,
            _session_id: Arc<str>,
            auth_header: Option<String>,
            _custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
        ) -> Result<(), StreamableHttpError<Self::Error>> {
            self.calls.lock().unwrap().push("delete_session");
            self.seen.lock().unwrap().push(auth_header);
            Ok(())
        }

        async fn get_stream(
            &self,
            _uri: Arc<str>,
            _session_id: Option<Arc<str>>,
            _last_event_id: Option<String>,
            auth_header: Option<String>,
            _custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
            StreamableHttpError<Self::Error>,
        > {
            self.calls.lock().unwrap().push("get_stream");
            self.seen.lock().unwrap().push(auth_header);
            Err(StreamableHttpError::Client(SpyError))
        }

        async fn get_stream_with_max_sse_event_size(
            &self,
            _uri: Arc<str>,
            _session_id: Option<Arc<str>>,
            _last_event_id: Option<String>,
            auth_header: Option<String>,
            _custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
            _max_sse_event_size: usize,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
            StreamableHttpError<Self::Error>,
        > {
            self.calls
                .lock()
                .unwrap()
                .push("get_stream_with_max_sse_event_size");
            self.seen.lock().unwrap().push(auth_header);
            Err(StreamableHttpError::Client(SpyError))
        }
    }

    /// Yields a different token each call, so a captured-once bug is visible.
    struct Rotating {
        n: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TokenProvider for Rotating {
        async fn token(&self) -> Result<Option<String>, ToolError> {
            let i = self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(format!("token-{i}")))
        }
    }

    fn empty_headers() -> std::collections::HashMap<http::HeaderName, http::HeaderValue> {
        std::collections::HashMap::new()
    }

    // This exact shape is lifted from rmcp's own `tests/test_custom_headers.rs`,
    // which drives `post_message` the same way this test does. Use it verbatim:
    // there is no `ClientJsonRpcMessage::notification` constructor, and guessing at
    // one costs more time than reading rmcp's test did.
    fn ping() -> rmcp::model::ClientJsonRpcMessage {
        use rmcp::model::{ClientRequest, PingRequest, RequestId};
        rmcp::model::ClientJsonRpcMessage::request(
            ClientRequest::PingRequest(PingRequest::default()),
            RequestId::Number(1),
        )
    }

    /// THE regression test for the frozen-header bug. Two requests through one
    /// client must carry two different tokens. Before this seam existed the header
    /// map was cloned from a config captured at connect time, so the second request
    /// necessarily reused the first token — which is exactly how a session died an
    /// hour in.
    #[tokio::test]
    async fn the_token_is_re_read_on_every_request() {
        let spy = SpyClient::default();
        let client = AuthedHttpClient::new(
            spy.clone(),
            Arc::new(Rotating {
                n: Default::default(),
            }),
        );

        let _ = client
            .post_message("http://x/".into(), ping(), None, None, empty_headers())
            .await;
        let _ = client
            .post_message("http://x/".into(), ping(), None, None, empty_headers())
            .await;

        let seen = spy.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![Some("token-0".to_string()), Some("token-1".to_string())],
            "the token was captured once instead of being re-read per request"
        );
    }

    /// `None` must not become an error or an empty string: it means the provider
    /// declines to override, so the caller's own `auth_header` (here, none) stands
    /// unchanged — which is what lets an unestablished session's 401 challenge be
    /// read and answered.
    #[tokio::test]
    async fn no_token_passes_the_callers_header_through_untouched() {
        let spy = SpyClient::default();
        let client = AuthedHttpClient::new(spy.clone(), Arc::new(NoToken));

        let _ = client
            .post_message("http://x/".into(), ping(), None, None, empty_headers())
            .await;
        let _ = client
            .post_message(
                "http://x/".into(),
                ping(),
                None,
                Some("caller-supplied".into()),
                empty_headers(),
            )
            .await;

        let seen = spy.seen.lock().unwrap().clone();
        assert_eq!(seen, vec![None, Some("caller-supplied".to_string())]);
    }

    /// Every method must consult the provider, not just the one that was easiest to
    /// wire. A stream or a session teardown carrying a stale token fails just as
    /// badly as a POST.
    #[tokio::test]
    async fn every_method_consults_the_provider() {
        let spy = SpyClient::default();
        let client = AuthedHttpClient::new(
            spy.clone(),
            Arc::new(Rotating {
                n: Default::default(),
            }),
        );

        let _ = client
            .post_message("http://x/".into(), ping(), None, None, empty_headers())
            .await;
        let _ = client
            .delete_session("http://x/".into(), "s".into(), None, empty_headers())
            .await;
        let _ = client
            .get_stream("http://x/".into(), None, None, None, empty_headers())
            .await;

        let seen = spy.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "a method skipped the provider");
        assert!(
            seen.iter().all(|h| h.is_some()),
            "a method sent no token despite the provider supplying one: {seen:?}"
        );
    }

    /// Answers a fixed, ordered sequence of results, one per `post_message` call,
    /// popped from the front of the queue. Built for the reactive-retry tests
    /// below, where a real server's first response (401) differs from its second
    /// (success) and a `SpyClient`-style "always fail" fake cannot express that.
    /// Only `post_message` is scripted; `delete_session` and `get_stream` panic if
    /// called, since the retry tests never reach them.
    type ScriptedResponse = Result<StreamableHttpPostResponse, StreamableHttpError<SpyError>>;

    #[derive(Clone)]
    struct SequencedClient {
        responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    }

    impl SequencedClient {
        fn new(responses: Vec<ScriptedResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            }
        }
    }

    impl StreamableHttpClient for SequencedClient {
        type Error = SpyError;

        async fn post_message(
            &self,
            _uri: Arc<str>,
            _message: ClientJsonRpcMessage,
            _session_id: Option<Arc<str>>,
            _auth_header: Option<String>,
            _custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
        ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("SequencedClient ran out of scripted responses")
        }

        async fn delete_session(
            &self,
            _uri: Arc<str>,
            _session_id: Arc<str>,
            _auth_header: Option<String>,
            _custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
        ) -> Result<(), StreamableHttpError<Self::Error>> {
            panic!("SequencedClient does not script delete_session; not exercised by these tests")
        }

        async fn get_stream(
            &self,
            _uri: Arc<str>,
            _session_id: Option<Arc<str>>,
            _last_event_id: Option<String>,
            _auth_header: Option<String>,
            _custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
            StreamableHttpError<Self::Error>,
        > {
            panic!("SequencedClient does not script get_stream; not exercised by these tests")
        }
    }

    /// Returns a fixed URI, mirroring the string literal every other test in this
    /// module inlines. A named helper matches the shape the task-1 brief's tests
    /// were written against.
    fn uri() -> Arc<str> {
        "http://x/".into()
    }

    #[tokio::test]
    async fn a_401_invalidates_the_token_and_the_request_is_retried_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Recorder {
            tokens_issued: AtomicUsize,
            invalidations: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl TokenProvider for Recorder {
            async fn token(&self) -> Result<Option<String>, ToolError> {
                let n = self.tokens_issued.fetch_add(1, Ordering::SeqCst);
                Ok(Some(format!("token-{n}")))
            }
            async fn invalidate(&self, _challenge: Option<&str>) {
                self.invalidations.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Answers 401 once, then succeeds.
        let inner = SequencedClient::new(vec![
            Err(StreamableHttpError::AuthRequired(
                rmcp::transport::streamable_http_client::AuthRequiredError::new(
                    "Bearer resource_metadata=\"https://example.test/.well-known/oauth-protected-resource\"".to_string(),
                ),
            )),
            Ok(StreamableHttpPostResponse::Accepted),
        ]);
        let provider = Arc::new(Recorder {
            tokens_issued: AtomicUsize::new(0),
            invalidations: AtomicUsize::new(0),
        });
        let client = AuthedHttpClient::new(inner, provider.clone());

        let result = client
            .post_message(uri(), ping(), None, None, empty_headers())
            .await;

        assert!(result.is_ok(), "the retry should have succeeded");
        assert_eq!(
            provider.invalidations.load(Ordering::SeqCst),
            1,
            "the 401 must reach the provider exactly once"
        );
        assert_eq!(
            provider.tokens_issued.load(Ordering::SeqCst),
            2,
            "a fresh token must be requested for the retry, not the stale one reused"
        );
    }

    #[tokio::test]
    async fn a_second_401_is_surfaced_rather_than_retried_forever() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Recorder {
            tokens_issued: AtomicUsize,
            invalidations: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl TokenProvider for Recorder {
            async fn token(&self) -> Result<Option<String>, ToolError> {
                let n = self.tokens_issued.fetch_add(1, Ordering::SeqCst);
                Ok(Some(format!("token-{n}")))
            }
            async fn invalidate(&self, _challenge: Option<&str>) {
                self.invalidations.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Two 401s in a row: one retry, then the challenge surfaces. A provider
        // that cannot recover must not spin.
        fn challenge() -> StreamableHttpError<SpyError> {
            StreamableHttpError::AuthRequired(
                rmcp::transport::streamable_http_client::AuthRequiredError::new(
                    "Bearer resource_metadata=\"https://example.test/.well-known/oauth-protected-resource\"".to_string(),
                ),
            )
        }

        let inner = SequencedClient::new(vec![Err(challenge()), Err(challenge())]);
        let provider = Arc::new(Recorder {
            tokens_issued: AtomicUsize::new(0),
            invalidations: AtomicUsize::new(0),
        });
        let client = AuthedHttpClient::new(inner, provider.clone());

        let result = client
            .post_message(uri(), ping(), None, None, empty_headers())
            .await;

        assert!(
            matches!(result, Err(StreamableHttpError::AuthRequired(_))),
            "a second 401 must surface, not retry forever: {result:?}"
        );
        assert_eq!(
            provider.invalidations.load(Ordering::SeqCst),
            1,
            "only the first 401 should have invalidated the token; the second must not retry again"
        );
        assert_eq!(
            provider.tokens_issued.load(Ordering::SeqCst),
            2,
            "one token for the original request, one for the single retry - no more"
        );
    }

    /// The `_with_max_sse_event_size` pair is the highest-risk spot for a silent
    /// regression: the trait gives them default bodies that delegate to the plain
    /// method on `Self`, so forwarding `AuthedHttpClient`'s variant to `inner`'s
    /// plain method instead of `inner`'s own `_with_max_sse_event_size` twin would
    /// still compile, still return a value, and would only show up as a dropped
    /// SSE-size limit in production. `SpyClient` overrides all five methods with
    /// distinct names, so a wrong forward here is caught by which name lands in
    /// `calls`, not just by whether *a* call landed.
    #[tokio::test]
    async fn every_method_including_the_max_sse_event_size_pair_reaches_its_exact_counterpart() {
        let spy = SpyClient::default();
        let client = AuthedHttpClient::new(
            spy.clone(),
            Arc::new(Rotating {
                n: Default::default(),
            }),
        );

        let _ = client
            .post_message("http://x/".into(), ping(), None, None, empty_headers())
            .await;
        let _ = client
            .post_message_with_max_sse_event_size(
                "http://x/".into(),
                ping(),
                None,
                None,
                empty_headers(),
                1024,
            )
            .await;
        let _ = client
            .delete_session("http://x/".into(), "s".into(), None, empty_headers())
            .await;
        let _ = client
            .get_stream("http://x/".into(), None, None, None, empty_headers())
            .await;
        let _ = client
            .get_stream_with_max_sse_event_size(
                "http://x/".into(),
                None,
                None,
                None,
                empty_headers(),
                1024,
            )
            .await;

        let calls = spy.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "post_message",
                "post_message_with_max_sse_event_size",
                "delete_session",
                "get_stream",
                "get_stream_with_max_sse_event_size",
            ],
            "a method was forwarded to the wrong counterpart on inner"
        );

        let seen = spy.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 5, "a method skipped the provider");
        assert!(
            seen.iter().all(|h| h.is_some()),
            "a method sent no token despite the provider supplying one: {seen:?}"
        );
    }
}
