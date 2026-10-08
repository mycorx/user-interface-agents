// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use std::sync::Arc;
use std::time::Duration;
use uia_core::tools::{ToolDescriptor, ToolError, ToolExecutor, ToolResult};

use crate::authed_client::AuthedHttpClient;
use crate::stderr_tail::{StderrTail, spawn_drain};
use crate::token::{NoToken, TokenProvider};

#[derive(Clone)]
pub enum McpServerConfig {
    /// Desktop only. Not available on iOS (sandbox forbids subprocess spawning)
    /// and heavily restricted on Android.
    ///
    /// `env` carries what a `.mcpb` manifest's `mcp_config.env` declared —
    /// part of what the bundle stated up front, so it travels with the
    /// command rather than being applied by the caller.
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// Available on every platform, and the only viable mobile transport.
    ///
    /// `headers` is the user-entered auth header (Claude Code's `-H`), sent
    /// on every request to this server and never logged.
    Http {
        url: String,
        headers: Vec<(String, String)>,
        /// True when this server's bearer token comes from a `TokenProvider`
        /// rather than a pasted header. Not a secret — it is a mode, not a
        /// credential — so it is safe in `Debug` output.
        expects_token: bool,
    },
}

impl McpServerConfig {
    pub fn is_desktop_only(&self) -> bool {
        matches!(self, McpServerConfig::Stdio { .. })
    }
}

/// The placeholder a header value is printed as. Header VALUES are the bearer
/// tokens this type exists to carry; header NAMES are not secret and are what a
/// debugger actually needs to see.
const REDACTED: &str = "<redacted>";

/// Hand-written rather than derived, ON PURPOSE — do not "tidy" this back into
/// `#[derive(Debug)]`.
///
/// A derived `Debug` would print an `Http` variant's auth token in full, so a
/// single `{:?}` anywhere downstream — a `tracing` field, an `unwrap` panic, a
/// `#[derive(Debug)]` on some future struct that merely *contains* one of these
/// — would put a live credential in a log file. The spec is that a remote
/// server's header value is never logged and never round-tripped into an error
/// message, and this impl is what enforces that for callers who never thought
/// about it.
///
/// `Stdio` prints the command, the env variable NAMES (values redacted) and
/// only the argument COUNT: a local server's env and args can carry keyring
/// secrets substituted from its `${user_config.*}` settings.
impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpServerConfig::Stdio { command, args, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", &format_args!("<{} redacted>", args.len()))
                .field(
                    "env",
                    &env.iter()
                        .map(|(name, _)| (name.as_str(), REDACTED))
                        .collect::<Vec<_>>(),
                )
                .finish(),
            McpServerConfig::Http {
                url,
                headers,
                expects_token,
            } => f
                .debug_struct("Http")
                .field("url", url)
                .field(
                    "headers",
                    &headers
                        .iter()
                        .map(|(name, _)| (name.as_str(), REDACTED))
                        .collect::<Vec<_>>(),
                )
                .field("expects_token", expects_token)
                .finish(),
        }
    }
}

/// Two MCP servers may expose a tool of the same name; namespacing keeps the
/// model's choice unambiguous.
pub fn namespaced(server: &str, tool: &str) -> String {
    format!("{server}.{tool}")
}

/// A server name becomes the first half of every one of its tools' names, and
/// OpenAI enforces `^[a-zA-Z0-9_-]+$` on tool names — rejecting the ENTIRE
/// `session.update` if one violates it, which silently disables every tool in
/// the session (S9 hit exactly this with the dot, and PLAN.md froze the `.`
/// -> `__` translation as the fix).
///
/// The dot is translated at the OpenAI boundary; nothing translates a `/` or a
/// `:`, so a server named after its URL would take every tool down with it.
/// Rejecting it here, at load time, is the only place the user can still fix it.
pub fn validate_server_name(name: &str) -> Result<(), ToolError> {
    if name.is_empty() {
        return Err(ToolError::Transport(
            "an MCP server name must not be empty".into(),
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
    {
        return Err(ToolError::Transport(format!(
            "MCP server name {name:?} contains {bad:?}; only [A-Za-z0-9_-] is \
             allowed, because the name prefixes every tool name and OpenAI \
             rejects the whole tool declaration otherwise"
        )));
    }
    Ok(())
}

/// What a connected server said it is, from the `initialize` handshake.
///
/// Every compliant MCP server answers with this, which is what makes a
/// remote server reviewable before it is trusted — there is no file to
/// inspect, so the protocol's own self-description is the declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerDeclaration {
    pub name: String,
    pub version: Option<String>,
}

/// Build the HTTP transport config, failing on a header the user can still
/// fix rather than at connect time.
///
/// Everything goes through `custom_headers`, including `Authorization`:
/// rmcp's separate `auth_header` field would mean branching on the header
/// name, and one path is easier to keep correct than two.
pub fn http_transport_config(
    url: &str,
    headers: &[(String, String)],
    expects_token: bool,
) -> Result<StreamableHttpClientTransportConfig, ToolError> {
    // A server is either static-header or OAuth, never both. rmcp applies the
    // per-request token with reqwest's `bearer_auth`, and custom headers are
    // applied afterwards with `HeaderMap::append` — APPEND, not insert — so
    // both values would travel and the server would see a malformed request.
    // The error names the header but never its value.
    if expects_token
        && headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
    {
        return Err(ToolError::Transport(
            "this server authenticates with OAuth, so it must not also carry a \
             static Authorization header - remove the header or switch the server \
             to static-header auth"
                .to_string(),
        ));
    }
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    for (name, value) in headers {
        let name = http::HeaderName::try_from(name.as_str()).map_err(|_| {
            ToolError::Transport(format!(
                "{name:?} is not a valid HTTP header name; only letters, digits and \
                 `-` are allowed"
            ))
        })?;
        // The value is deliberately not quoted into the message: it is the
        // secret this whole field exists to carry.
        let value = http::HeaderValue::try_from(value.as_str()).map_err(|_| {
            ToolError::Transport(format!(
                "the value given for the {name} header is not a valid HTTP header value \
                 (a stray newline or control character is the usual cause)"
            ))
        })?;
        config.custom_headers.insert(name, value);
    }
    Ok(config)
}

/// Recovers a `WWW-Authenticate` challenge from a failed `initialize`
/// handshake, if that is why it failed.
///
/// `rmcp::service::ClientInitializeError::TransportError` boxes the transport
/// error as `Box<dyn std::error::Error + Send + Sync>` (its `DynamicTransportError`
/// payload's `error` field is public exactly so a caller can walk it), so this
/// downcasts through the `source()` chain looking for the one concrete type
/// that carries the header: `StreamableHttpError::AuthRequired`'s own
/// `#[source] AuthRequiredError`. `StreamableHttpError` never appears in this
/// function's signature — only the extracted `String` crosses back out — so
/// nothing about this leaks the boundary this module exists to hold.
fn auth_challenge_from_initialize_error(
    err: &rmcp::service::ClientInitializeError,
) -> Option<String> {
    let rmcp::service::ClientInitializeError::TransportError { error, .. } = err else {
        return None;
    };
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error.error.as_ref());
    while let Some(e) = current {
        if let Some(auth_required) =
            e.downcast_ref::<rmcp::transport::streamable_http_client::AuthRequiredError>()
        {
            return Some(auth_required.www_authenticate_header.clone());
        }
        current = e.source();
    }
    None
}

pub struct McpExecutor {
    server_name: String,
    service: RunningService<RoleClient, ()>,
}

impl McpExecutor {
    /// `name` is the configured, user-facing identifier — NOT the command or
    /// URL. Deriving it from the transport (as this did before S14) namespaces
    /// an HTTP server's tools as `http://host:9000/mcp.tool`, which OpenAI
    /// rejects outright. Validated here so the failure is a clear config error
    /// rather than a session in which no tool works.
    ///
    /// `provider` is consulted on every HTTP request via `AuthedHttpClient`; the
    /// `Stdio` arm ignores it entirely — a subprocess has no HTTP headers to
    /// attach a bearer token to.
    pub async fn connect_with_token_provider(
        name: &str,
        config: McpServerConfig,
        provider: Arc<dyn TokenProvider>,
    ) -> Result<Self, ToolError> {
        validate_server_name(name)?;
        let server_name = name.to_string();
        match config {
            McpServerConfig::Stdio { command, args, env } => {
                let mut cmd = tokio::process::Command::new(&command);
                cmd.args(&args);
                for (key, value) in &env {
                    cmd.env(key, value);
                }
                // Pipe the child's stderr instead of inheriting it: a server that
                // dies at startup says why on stderr, and an inherited stream is
                // invisible to Settings. The drain below keeps echoing each line
                // to our own stderr, so terminal behavior is unchanged.
                let (transport, stderr) = TokioChildProcess::builder(cmd)
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| ToolError::Transport(e.to_string()))?;
                let tail = StderrTail::new(env.iter().map(|(_, v)| v.clone()).collect());
                let drain =
                    stderr.map(|stderr| spawn_drain(stderr, tail.clone(), server_name.clone()));
                let service = match ().serve(transport).await {
                    Ok(service) => service,
                    Err(e) => {
                        // The child has usually exited by now, so its stream
                        // ends and the drain finishes; bound the wait anyway
                        // in case it is still alive.
                        if let Some(drain) = drain {
                            let _ = tokio::time::timeout(Duration::from_millis(500), drain).await;
                        }
                        let printed = tail.render();
                        let message = if printed.is_empty() {
                            e.to_string()
                        } else {
                            format!("{e}; the server printed: {printed}")
                        };
                        return Err(ToolError::Transport(message));
                    }
                };
                Ok(Self {
                    server_name,
                    service,
                })
            }
            McpServerConfig::Http {
                url,
                headers,
                expects_token,
            } => {
                let config = http_transport_config(&url, &headers, expects_token)?;
                // `with_client`, not `from_config`: the wrapper is what turns rmcp's
                // per-call `auth_header` parameter into a live token. `from_config`
                // would use the default client, whose only auth inputs are the
                // config's own frozen fields.
                //
                // The client below is NOT `reqwest::Client::default()`. rmcp's own
                // `from_config`/`from_uri` route through a private `default_http_client()`
                // that sets two things a bare default client does not:
                //
                //   - `pool_max_idle_per_host(0)`: avoids ~40ms stalls from TCP Delayed
                //     ACK on Linux when a pooled connection is reused before the previous
                //     response body was fully consumed.
                //   - `redirect(Policy::none())`: reqwest's `remove_sensitive_headers`
                //     only strips `Authorization`, `Cookie` and `Proxy-Authorization` when
                //     following a redirect. Any other caller-supplied header — including
                //     an `X-API-Key` this repo stores in `custom_headers` — would be
                //     replayed verbatim to whatever host a redirect points at. Disabling
                //     redirects is what keeps that from happening.
                //
                // Because `default_http_client()` is private to rmcp, we cannot call it
                // directly; these two settings are reproduced here deliberately and must
                // not be "simplified" back to `reqwest::Client::default()` or `::new()`.
                let http_client = reqwest::Client::builder()
                    .pool_max_idle_per_host(0)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|e| ToolError::Transport(e.to_string()))?;
                let transport = StreamableHttpClientTransport::with_client(
                    AuthedHttpClient::new(http_client, provider),
                    config,
                );
                let service = ().serve(transport).await.map_err(|e| {
                    // A bare `e.to_string()` throws the `WWW-Authenticate`
                    // header away: `ClientInitializeError::TransportError`'s
                    // `Display` names its context, not the header inside the
                    // boxed transport error, and `StreamableHttpError::
                    // AuthRequired`'s own `#[error("Auth required")]` carries
                    // none of it either — the header lives only in the
                    // `#[source]` `AuthRequiredError` payload. A caller that
                    // wants to react to "this server needs OAuth" (rather
                    // than report "unreachable") needs that header, so it is
                    // recovered here, behind the one boundary that already
                    // depends on rmcp, and handed across as a plain `String`.
                    auth_challenge_from_initialize_error(&e)
                        .map(ToolError::AuthRequired)
                        .unwrap_or_else(|| ToolError::Transport(e.to_string()))
                })?;
                Ok(Self {
                    server_name,
                    service,
                })
            }
        }
    }

    /// Connect with no bearer token of its own.
    ///
    /// Servers configured with an arbitrary static header are unaffected: that
    /// header travels in `custom_headers`, which this path does not touch.
    pub async fn connect(name: &str, config: McpServerConfig) -> Result<Self, ToolError> {
        Self::connect_with_token_provider(name, config, Arc::new(NoToken)).await
    }

    /// What this server declared about itself at `initialize`.
    ///
    /// `None` before the handshake completes — which cannot happen on a value
    /// `connect` returned — and also when the peer supplied no implementation
    /// identity at all: rmcp's `ServerPeerInfo::server_info` is itself an
    /// `Option<Implementation>`, because a discovery response is not required
    /// to name itself. A server that declines to say what it is therefore has
    /// no declaration to review, which is exactly what `None` means here.
    pub fn declaration(&self) -> Option<ServerDeclaration> {
        let info = self.service.peer_info()?;
        let implementation = info.server_info.as_ref()?;
        Some(ServerDeclaration {
            name: implementation.name.clone(),
            version: Some(implementation.version.clone()),
        })
    }
}

#[async_trait]
impl ToolExecutor for McpExecutor {
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|e| ToolError::Transport(e.to_string()))?;
        Ok(tools
            .into_iter()
            .map(|t| ToolDescriptor {
                name: namespaced(&self.server_name, &t.name),
                description: t.description.unwrap_or_default().to_string(),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
                requires_confirmation: false,
            })
            .collect())
    }

    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        deadline: Duration,
    ) -> Result<ToolResult, ToolError> {
        // Strip the namespace before calling the server.
        let bare = name.split_once('.').map(|(_, t)| t).unwrap_or(name);
        let mut params = CallToolRequestParams::new(bare.to_string());
        if let Some(obj) = args.as_object() {
            params = params.with_arguments(obj.clone());
        }
        let call = self.service.call_tool(params);
        match tokio::time::timeout(deadline, call).await {
            Err(_) => Err(ToolError::Timeout(deadline)),
            Ok(Err(e)) => Err(ToolError::Transport(e.to_string())),
            Ok(Ok(res)) => {
                let text = res
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(if res.is_error.unwrap_or(false) {
                    ToolResult::error(text)
                } else {
                    ToolResult::ok(text)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_config_is_desktop_only() {
        // iOS forbids subprocess spawning and Android heavily restricts it, so
        // remote HTTP is the only viable mobile tool transport.
        let cfg = McpServerConfig::Stdio {
            command: "echo".into(),
            args: vec![],
            env: vec![],
        };
        assert!(cfg.is_desktop_only());
        let http = McpServerConfig::Http {
            url: "http://localhost:9000".into(),
            headers: vec![],
            expects_token: false,
        };
        assert!(!http.is_desktop_only());
    }

    /// `McpServerConfig`'s `Debug` is hand-written specifically so this holds.
    /// If someone replaces that impl with `#[derive(Debug)]` — which looks like
    /// a harmless cleanup — this test is what catches it before a bearer token
    /// reaches a log file. The spec is that a header value is never logged and
    /// never round-tripped into an error message; a single `{:?}` in any
    /// downstream crate is enough to break that, and callers do not read the
    /// enum's doc comment before formatting it.
    #[test]
    fn debug_output_redacts_a_header_value_but_keeps_the_name() {
        let cfg = McpServerConfig::Http {
            url: "https://example.test/mcp".into(),
            headers: vec![("Authorization".into(), "Bearer super-secret-token".into())],
            expects_token: false,
        };
        let printed = format!("{cfg:?}");

        assert!(
            !printed.contains("super-secret-token"),
            "the auth token leaked into Debug output: {printed}"
        );
        // Still useful to debug with: you can see WHICH header was sent, and where.
        assert!(printed.contains("https://example.test/mcp"), "{printed}");
        assert!(printed.contains("Authorization"), "{printed}");
        assert!(printed.contains(REDACTED), "{printed}");
    }

    /// A stdio server's env can carry a keyring secret (a substituted
    /// `${user_config.*}` value), so only the variable NAMES are printed.
    #[test]
    fn a_stdio_configs_env_values_are_never_printed() {
        let cfg = McpServerConfig::Stdio {
            command: "uvx".into(),
            args: vec!["mcp-server-time".into()],
            env: vec![("API_TOKEN".into(), "sk-super-secret-value".into())],
        };
        let printed = format!("{cfg:?}");
        assert!(printed.contains("uvx"), "{printed}");
        assert!(!printed.contains("mcp-server-time"), "{printed}");
        assert!(printed.contains("API_TOKEN"), "{printed}");
        assert!(!printed.contains("sk-super-secret-value"), "{printed}");
        assert!(printed.contains(REDACTED), "{printed}");
    }

    /// A remote server's auth header is the whole reason a private MCP
    /// endpoint is reachable at all; dropping it silently would look like
    /// "the server rejected us" rather than "we never sent the token".
    #[test]
    fn a_configured_header_reaches_the_transport_config() {
        let cfg = http_transport_config(
            "https://example.test/mcp",
            &[("Authorization".into(), "Bearer secret-token".into())],
            false,
        )
        .unwrap();

        assert_eq!(&*cfg.uri, "https://example.test/mcp");
        let sent = cfg
            .custom_headers
            .get(&http::HeaderName::from_static("authorization"))
            .expect("the Authorization header was dropped");
        assert_eq!(sent.to_str().unwrap(), "Bearer secret-token");
    }

    #[test]
    fn no_headers_means_an_empty_header_map_not_a_failure() {
        let cfg = http_transport_config("https://example.test/mcp", &[], false).unwrap();
        assert!(cfg.custom_headers.is_empty());
    }

    #[test]
    fn an_authorization_custom_header_is_refused_when_the_server_uses_oauth() {
        // Both mechanisms target the same header, and reqwest APPENDS rather than
        // replaces — so this would put two Authorization headers on the wire and
        // the gateway would reject the request with a confusing 400. Refusing at
        // config time turns a mystery into a message.
        let err = http_transport_config(
            "https://example.test/mcp",
            &[("Authorization".to_string(), "Bearer pasted".to_string())],
            true, // this server authenticates via OAuth
        )
        .expect_err("a collision must be refused, not silently sent twice");

        let msg = err.to_string();
        assert!(msg.contains("Authorization"), "got {msg}");
        assert!(
            !msg.contains("pasted"),
            "the error must not quote the secret: {msg}"
        );
    }

    #[test]
    fn a_non_authorization_header_is_still_fine_alongside_oauth() {
        // Only the Authorization collision is a problem. An X-Tenant-Id or similar
        // travels in custom_headers and never contends with the bearer token.
        let config = http_transport_config(
            "https://example.test/mcp",
            &[("X-Tenant-Id".to_string(), "acme".to_string())],
            true,
        )
        .expect("a non-colliding header must still be allowed");
        assert_eq!(config.custom_headers.len(), 1);
    }

    #[test]
    fn an_authorization_header_is_still_allowed_when_there_is_no_oauth() {
        // The existing PAT path. This is the regression guard: pasting
        // `Authorization: Bearer <PAT>` must keep working exactly as before.
        let config = http_transport_config(
            "https://example.test/mcp",
            &[("Authorization".to_string(), "Bearer pat".to_string())],
            false,
        )
        .expect("the static-header path must be untouched");
        assert_eq!(config.custom_headers.len(), 1);
    }

    /// A pasted header with a newline in it is a request-splitting shape,
    /// and a blank name is a typo. Both must fail where the user can still
    /// fix them, not at connect time.
    #[test]
    fn a_malformed_header_is_rejected_rather_than_silently_dropped() {
        assert!(
            http_transport_config(
                "https://example.test/mcp",
                &[("Bad Name".into(), "value".into())],
                false,
            )
            .is_err()
        );
        assert!(
            http_transport_config(
                "https://example.test/mcp",
                &[("Authorization".into(), "Bearer x\r\nX-Evil: 1".into())],
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn tool_names_are_namespaced_by_server() {
        assert_eq!(namespaced("clock", "now"), "clock.now");
    }

    #[test]
    fn a_server_name_that_would_break_openais_tool_declaration_is_rejected() {
        // The real regression this guards: before S14 an HTTP server was named
        // after its own URL, and every one of its tools inherited the `:` and
        // `/` — which makes OpenAI reject the whole `session.update` and
        // silently disables EVERY tool in the session, not just that server's.
        assert!(validate_server_name("http://localhost:9000/mcp").is_err());
        assert!(validate_server_name("uvx mcp-server-time").is_err());
        assert!(validate_server_name("clock.local").is_err());
        assert!(validate_server_name("").is_err());
    }

    #[test]
    fn ordinary_server_names_are_accepted() {
        for name in ["clock", "file-system", "my_tools", "srv2"] {
            assert!(validate_server_name(name).is_ok(), "{name} was rejected");
        }
    }

    /// The HTTP transport must be built through `AuthedHttpClient`, or the provider is
    /// decorative: a token that is fetched but never reaches a request changes nothing.
    /// This asserts the wiring exists at the type level, which is what a later OAuth
    /// provider depends on.
    #[tokio::test]
    async fn connect_with_a_token_provider_is_available_for_http() {
        // A dead port: this test is about the construction path, not about reaching a
        // server, so "did not succeed" is the expected outcome. The timeout guards
        // against rmcp's own retry/backoff turning a failure into a hang.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            McpExecutor::connect_with_token_provider(
                "unreachable",
                McpServerConfig::Http {
                    url: "http://127.0.0.1:1/mcp".into(),
                    headers: vec![],
                    expects_token: false,
                },
                std::sync::Arc::new(NoToken),
            ),
        )
        .await;

        // Either it errored, or it was still retrying when the timeout fired. Both mean
        // it did not connect; neither is allowed to be a success.
        match outcome {
            Ok(result) => assert!(result.is_err(), "a server on a dead port must not connect"),
            Err(_elapsed) => {}
        }
    }

    /// Every existing caller keeps working: `connect` still exists and still takes two
    /// arguments. If this stops compiling, the refactor broke the public API.
    #[tokio::test]
    async fn plain_connect_still_exists_for_existing_callers() {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            McpExecutor::connect(
                "unreachable",
                McpServerConfig::Http {
                    url: "http://127.0.0.1:1/mcp".into(),
                    headers: vec![],
                    expects_token: false,
                },
            ),
        )
        .await;

        match outcome {
            Ok(result) => assert!(result.is_err()),
            Err(_elapsed) => {}
        }
    }

    /// THE regression test for Fix 1: a custom header (e.g. `X-API-Key`) must never
    /// reach a redirect target. Before this fix, `connect_with_token_provider` built
    /// its `reqwest::Client` with `Client::default()`, which follows redirects — and
    /// reqwest's `remove_sensitive_headers` only strips `Authorization`, `Cookie` and
    /// `Proxy-Authorization`, not arbitrary custom headers. Structured after rmcp's
    /// own `default_http_client_does_not_leak_custom_headers_to_redirect_target`
    /// test (`rmcp-3.1.4/src/transport/common/reqwest/streamable_http_client.rs`),
    /// but driven through the real `connect_with_token_provider` path so it also
    /// catches a regression in how *this* crate builds its client, not just in rmcp.
    #[tokio::test]
    async fn connecting_does_not_leak_a_custom_header_to_a_redirect_target() {
        use axum::Router;
        use axum::extract::State;
        use axum::http::{HeaderMap, StatusCode, header::LOCATION};
        use axum::routing::post;
        use std::net::SocketAddr;
        use std::sync::Mutex as StdMutex;

        const API_KEY_HEADER: &str = "x-api-key";
        const API_KEY_VALUE: &str = "top-secret-value";

        type Captured = Arc<StdMutex<Option<String>>>;

        fn capture(headers: &HeaderMap, into: &Captured) {
            if let Some(v) = headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok()) {
                *into.lock().unwrap() = Some(v.to_owned());
            }
        }

        // The redirect target: if this is ever hit, the fix failed. It would also
        // capture the header if it arrived, so a future change to "just don't
        // redirect, but still leak" would be caught too.
        let redirected_seen: Captured = Arc::new(StdMutex::new(None));
        let redirected_hit = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let redirected_listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let redirected_addr = redirected_listener.local_addr().unwrap();
        let redirected_server = tokio::spawn({
            let redirected_seen = redirected_seen.clone();
            let redirected_hit = redirected_hit.clone();
            async move {
                let app = Router::new()
                    .route(
                        "/capture",
                        post(
                            move |State((seen, hit)): State<(
                                Captured,
                                Arc<std::sync::atomic::AtomicUsize>,
                            )>,
                                  headers: HeaderMap| async move {
                                hit.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                capture(&headers, &seen);
                                (
                                    StatusCode::OK,
                                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                                    r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                                )
                            },
                        ),
                    )
                    .with_state((redirected_seen, redirected_hit));
                axum::serve(redirected_listener, app).await
            }
        });

        // The server the config points at: redirects every POST to the target above.
        let original_seen: Captured = Arc::new(StdMutex::new(None));
        let redirect_listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let redirect_addr = redirect_listener.local_addr().unwrap();
        let location = format!("http://{redirected_addr}/capture");
        let redirect_server = tokio::spawn({
            let original_seen = original_seen.clone();
            async move {
                let app = Router::new()
                    .route(
                        "/mcp",
                        post(
                            move |State((seen, location)): State<(Captured, String)>,
                                  headers: HeaderMap| async move {
                                capture(&headers, &seen);
                                (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, location)], "")
                            },
                        ),
                    )
                    .with_state((original_seen, location));
                axum::serve(redirect_listener, app).await
            }
        });

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            McpExecutor::connect_with_token_provider(
                "redirect-test",
                McpServerConfig::Http {
                    url: format!("http://{redirect_addr}/mcp"),
                    headers: vec![(API_KEY_HEADER.into(), API_KEY_VALUE.into())],
                    expects_token: false,
                },
                Arc::new(NoToken),
            ),
        )
        .await;

        redirect_server.abort();
        redirected_server.abort();

        // A redirect is disabled, so the initialize handshake must fail rather than
        // silently succeed via the redirect target.
        match outcome {
            Ok(result) => assert!(
                result.is_err(),
                "connecting through a server that only ever redirects must not succeed"
            ),
            Err(_elapsed) => {}
        }

        assert_eq!(
            original_seen.lock().unwrap().as_deref(),
            Some(API_KEY_VALUE),
            "the original server should still see the header — only replay to the \
             redirect target is the problem"
        );
        assert_eq!(
            redirected_hit.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the redirect target must never be reached at all: redirects are disabled"
        );
        assert!(
            redirected_seen.lock().unwrap().is_none(),
            "the custom header leaked to the redirect target"
        );
    }

    /// Connects `/bin/sh -c script` (which never speaks MCP, so the handshake
    /// fails) and returns the transport error text.
    #[cfg(unix)]
    async fn failed_startup_message(script: &str, env: Vec<(String, String)>) -> String {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            McpExecutor::connect(
                "probe",
                McpServerConfig::Stdio {
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), script.into()],
                    env,
                },
            ),
        )
        .await
        .expect("connect must not hang on a chatty server");
        match result {
            Err(ToolError::Transport(msg)) => msg,
            Err(other) => panic!("expected a Transport error, got {other:?}"),
            Ok(_) => panic!("a script that never speaks MCP must not connect"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_startup_shows_what_the_server_printed() {
        let msg =
            failed_startup_message("echo 'cannot read credentials.json' >&2; exit 3", vec![]).await;
        assert!(msg.contains("cannot read credentials.json"), "{msg}");
        assert!(msg.contains("the server printed"), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_launch_env_secret_is_redacted_from_the_startup_output() {
        let msg = failed_startup_message(
            "echo \"token is $TOKEN\" >&2; exit 1",
            vec![("TOKEN".into(), "supersecret-value-123".into())],
        )
        .await;
        assert!(msg.contains("token is"), "{msg}");
        assert!(msg.contains("<redacted>"), "{msg}");
        assert!(!msg.contains("supersecret-value-123"), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_very_chatty_server_never_blocks_on_a_full_stderr_pipe() {
        let msg = failed_startup_message(
            "i=0; while [ $i -lt 20000 ]; do echo line-$i >&2; i=$((i+1)); done; exit 1",
            vec![],
        )
        .await;
        assert!(msg.contains("line-19999"), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_cannot_be_spawned_has_no_server_output_to_show() {
        let result = McpExecutor::connect(
            "probe",
            McpServerConfig::Stdio {
                command: "/nonexistent/uia-no-such-server".into(),
                args: vec![],
                env: vec![],
            },
        )
        .await;
        match result {
            Err(ToolError::Transport(msg)) => assert!(!msg.contains("the server printed"), "{msg}"),
            Err(other) => panic!("expected a Transport error, got {other:?}"),
            Ok(_) => panic!("a missing command must not connect"),
        }
    }

    #[tokio::test]
    #[ignore = "spawns a real MCP server process; run manually"]
    async fn stdio_server_lists_its_tools() {
        let exec = McpExecutor::connect(
            "everything",
            McpServerConfig::Stdio {
                command: "npx".into(),
                args: vec![
                    "-y".into(),
                    "@modelcontextprotocol/server-everything".into(),
                ],
                env: vec![],
            },
        )
        .await
        .unwrap();
        assert!(!exec.list_tools().await.unwrap().is_empty());
    }
}
