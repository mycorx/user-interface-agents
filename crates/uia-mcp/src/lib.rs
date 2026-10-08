// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

pub mod authed_client;
pub mod bundle;
pub mod callback;
pub mod client;
pub mod oauth;
pub mod router;
mod stderr_tail;
pub mod token;
pub mod translate;
pub mod wire_names;

pub use authed_client::AuthedHttpClient;
pub use bundle::BundleError;
pub use bundle::{InstalledBundle, install_bundle};
pub use client::{
    McpExecutor, McpServerConfig, ServerDeclaration, http_transport_config, namespaced,
    validate_server_name,
};
pub use oauth::OAuthProvider;
pub use router::{ListingObserver, ListingOutcome, McpRouter};
pub use token::{NoToken, TokenProvider};
pub use translate::{to_nova_declaration, to_openai_declaration};
pub use wire_names::{ToolNames, wire_tool_name};

/// rmcp's OAuth types, re-exported so `uia-app` can implement a credential
/// store without taking a direct `rmcp` dependency. One crate owns the MCP SDK
/// boundary; that crate is this one.
///
/// `AuthorizationManager`/`AuthorizationRequest`/`AuthorizationSession` were
/// added for the `preview_remote` two-phase OAuth flow (Task 7): building the
/// browser authorize URL and driving the callback exchange both need these
/// types, and the alternative — teaching `uia-mcp` to do that work itself —
/// would smuggle Tauri-flavored orchestration (a pending-session stash keyed
/// by CSRF `state`) into a crate that has no business knowing about it.
pub mod auth {
    pub use oauth2::{RefreshToken, TokenResponse};
    pub use rmcp::transport::auth::{
        AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession,
        CredentialStore, StoredCredentials, VendorExtraTokenFields,
    };
}
