// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Persists an MCP server's OAuth credentials in the OS keychain.
//!
//! rmcp's `CredentialStore` is async; `SecretStore` is sync and backed by
//! `keyring`. The bridge is deliberate: keyring calls are fast, local, and
//! blocking, and wrapping them in `spawn_blocking` would buy nothing but a
//! thread hop. If that ever stops being true, this is the one place to change.

use std::sync::Arc;

use async_trait::async_trait;
use uia_mcp::auth::{
    AuthError, CredentialStore, RefreshToken, StoredCredentials, TokenResponse,
    VendorExtraTokenFields,
};

use crate::secrets::SecretStore;

pub struct KeyringCredentialStore {
    secrets: Arc<dyn SecretStore>,
    account: String,
    /// Cognito's `refresh_token` alone routinely runs past Windows
    /// Credential Manager's real per-credential limit (see
    /// `CHUNK_LIMIT_UTF16`), so it is stored separately from the rest of
    /// `StoredCredentials` under this base name, both of them chunked.
    refresh_account: String,
}

/// Windows Credential Manager caps a single credential's password at
/// `CRED_MAX_CREDENTIAL_BLOB_SIZE` — 2560 *bytes*, i.e. 1280 UTF-16 code
/// units, not the 2560 the `keyring` crate's own error text calls "chars"
/// (its check is `password.encode_utf16().count() * 2 > 2560`, so the real
/// ceiling is exactly half that). Confirmed live: a 1259-unit blob saved
/// fine, a 1660-unit one failed with that same mislabeled message. A real
/// Cognito refresh token alone (~1600 units) already exceeds the real
/// limit, so nothing here can assume one entry is enough — every value
/// this store persists is split into chunks safely under it, on every
/// platform `keyring` supports (not just Windows, so the margin is never
/// wasted work elsewhere).
const CHUNK_LIMIT_UTF16: usize = 1000;

fn chunk_account(base_account: &str, index: usize) -> String {
    format!("{base_account}.{index}")
}

fn chunk_count_account(base_account: &str) -> String {
    format!("{base_account}.count")
}

/// Hand-written: a derived `Debug` would print nothing secret today, but this
/// type exists to hold credentials and the derive would follow any field added
/// later straight into a log line.
impl std::fmt::Debug for KeyringCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringCredentialStore")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

impl KeyringCredentialStore {
    /// Keyed per server, so two remote servers never share a token.
    pub fn new(secrets: Arc<dyn SecretStore>, server_name: &str) -> Self {
        let account = format!("mcp-oauth.{server_name}");
        let refresh_account = format!("{account}.refresh");
        Self {
            secrets,
            account,
            refresh_account,
        }
    }

    /// Writes `value` across as many `{base_account}.0`, `{base_account}.1`,
    /// ... entries as it takes to stay under `CHUNK_LIMIT_UTF16` each, plus a
    /// `{base_account}.count` entry recording how many. Chunking on raw
    /// UTF-16 units (rather than `char`s) risks splitting a surrogate pair
    /// across chunks, but every value stored here is an opaque OAuth
    /// token/JWT — ASCII by construction — so that never arises in practice.
    fn store_chunked(
        &self,
        base_account: &str,
        value: &str,
    ) -> Result<(), crate::secrets::SecretStoreError> {
        let units: Vec<u16> = value.encode_utf16().collect();
        let chunks: Vec<String> = units
            .chunks(CHUNK_LIMIT_UTF16)
            .map(String::from_utf16_lossy)
            .collect();
        // Best-effort cleanup of any leftover higher-index chunks from a
        // previous, longer value — orphaned entries are harmless (`count`
        // below is what `load_chunked` actually trusts) but there's no
        // reason to leave them around.
        if let Some(previous_count) = self
            .secrets
            .get(&chunk_count_account(base_account))
            .and_then(|c| c.parse::<usize>().ok())
        {
            for i in chunks.len()..previous_count {
                let _ = self.secrets.delete(&chunk_account(base_account, i));
            }
        }
        for (i, chunk) in chunks.iter().enumerate() {
            self.secrets.set(&chunk_account(base_account, i), chunk)?;
        }
        self.secrets.set(
            &chunk_count_account(base_account),
            &chunks.len().to_string(),
        )
    }

    /// Reassembles a value `store_chunked` wrote. `None` if nothing was ever
    /// stored under this base name, or if a chunk went missing (treated the
    /// same as "not signed in" everywhere else in this store).
    fn load_chunked(&self, base_account: &str) -> Option<String> {
        let count: usize = self
            .secrets
            .get(&chunk_count_account(base_account))?
            .parse()
            .ok()?;
        let mut value = String::new();
        for i in 0..count {
            value.push_str(&self.secrets.get(&chunk_account(base_account, i))?);
        }
        Some(value)
    }

    /// Deletes every chunk (and the count entry) under `base_account`.
    /// Best-effort per chunk: a chunk that never existed isn't a failure,
    /// only the count entry's own delete result is reported.
    fn clear_chunked(&self, base_account: &str) -> Result<(), crate::secrets::SecretStoreError> {
        if let Some(count) = self
            .secrets
            .get(&chunk_count_account(base_account))
            .and_then(|c| c.parse::<usize>().ok())
        {
            for i in 0..count {
                let _ = self.secrets.delete(&chunk_account(base_account, i));
            }
        }
        self.secrets.delete(&chunk_count_account(base_account))
    }
}

#[async_trait]
impl CredentialStore for KeyringCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        // Both "no entry" and "keychain unavailable" arrive here as `None`,
        // and both mean the same thing to the caller: not signed in. Surfacing
        // the difference would only offer a crash where a prompt belongs.
        let Some(raw) = self.load_chunked(&self.account) else {
            return Ok(None);
        };
        // A corrupt entry is also "not signed in" — re-authorizing fixes it,
        // and failing here would wedge the user with no way forward.
        let Some(mut credentials) = serde_json::from_str::<StoredCredentials>(&raw).ok() else {
            return Ok(None);
        };
        // `save` splits the refresh token into its own (also chunked) entry
        // (see there for why); splice it back onto the token response the
        // caller expects. A missing refresh entry is not an error — a token
        // response with no refresh token is a valid state `rmcp` already
        // handles.
        if let Some(refresh) = self.load_chunked(&self.refresh_account) {
            if let Some(token_response) = credentials.token_response.as_mut() {
                token_response.set_refresh_token(Some(RefreshToken::new(refresh)));
            }
        }
        Ok(Some(credentials))
    }

    async fn save(&self, mut credentials: StoredCredentials) -> Result<(), AuthError> {
        // Cognito's token response includes an `id_token` (since scope always
        // includes `openid`) alongside `access_token`/`refresh_token`, all
        // three full JWTs/opaque tokens. Only `access_token` and
        // `refresh_token` are ever read back (`get_access_token`/
        // `refresh_token` in rmcp's `AuthorizationManager`) — nothing here
        // uses `id_token`'s OIDC claims, so it is dropped outright. Even
        // without it, and even split from `access_token`, a real Cognito
        // `refresh_token` alone can still exceed a single Windows credential
        // entry's real limit (see `CHUNK_LIMIT_UTF16`) — so both the
        // refresh token and the rest of `credentials` are stored chunked,
        // not just split from each other.
        let refresh_token = credentials
            .token_response
            .as_mut()
            .and_then(|token_response| {
                token_response.set_extra_fields(VendorExtraTokenFields::default());
                let refresh = token_response.refresh_token().map(|t| t.secret().clone());
                token_response.set_refresh_token(None);
                refresh
            });

        let raw = serde_json::to_string(&credentials).map_err(|e| {
            AuthError::InternalError(format!("could not serialize credentials: {e}"))
        })?;
        // `SecretStoreError`'s Display names the backend failure, never the
        // value — checked before relying on it here.
        let store_err = |e: crate::secrets::SecretStoreError| {
            AuthError::InternalError(format!("could not store credentials: {e}"))
        };

        match refresh_token {
            Some(refresh) => self
                .store_chunked(&self.refresh_account, &refresh)
                .map_err(store_err)?,
            // No refresh token in this response (unusual, but valid): drop
            // any stale one from a previous sign-in rather than leaving it
            // to be spliced onto credentials it no longer belongs to.
            None => {
                let _ = self.clear_chunked(&self.refresh_account);
            }
        }
        self.store_chunked(&self.account, &raw).map_err(store_err)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let clear_err = |e: crate::secrets::SecretStoreError| {
            AuthError::InternalError(format!("could not clear credentials: {e}"))
        };
        // Best-effort on the refresh entry: it may never have existed, and a
        // missing account isn't the failure this needs to report — the main
        // entry's own delete below is what determines whether `clear` failed.
        let _ = self.clear_chunked(&self.refresh_account);
        self.clear_chunked(&self.account).map_err(clear_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{FakeSecretStore, SecretStore, SecretStoreError};
    use std::sync::Arc;
    use uia_mcp::auth::StoredCredentials;

    fn creds() -> StoredCredentials {
        // `StoredCredentials` is `#[non_exhaustive]` upstream, so it is built
        // through its constructor rather than a struct literal.
        StoredCredentials::new(
            "exampleclientid0123456789a".to_string(),
            None,
            vec!["openid".to_string()],
            Some(1_700_000_000),
        )
        .with_issuer(Some("https://example.test".to_string()))
    }

    /// A `SecretStore` whose backend is always down: `get` reports no entry,
    /// `set`/`delete` fail. Exercises `KeyringCredentialStore`'s posture that
    /// an unreadable keychain reads as "not signed in", never as a crash.
    struct FailingSecretStore;

    impl SecretStore for FailingSecretStore {
        fn get(&self, _account: &str) -> Option<String> {
            None
        }

        fn set(&self, _account: &str, _value: &str) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::Backend(
                "keychain unavailable".to_string(),
            ))
        }

        fn delete(&self, _account: &str) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::Backend(
                "keychain unavailable".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn credentials_survive_a_save_and_load_round_trip() {
        let secrets = Arc::new(FakeSecretStore::new());
        let store = KeyringCredentialStore::new(secrets, "example-server");
        assert!(store.load().await.unwrap().is_none(), "nothing stored yet");

        store.save(creds()).await.unwrap();
        let loaded = store.load().await.unwrap().expect("just saved");
        assert_eq!(loaded.client_id, creds().client_id);
    }

    #[tokio::test]
    async fn two_servers_never_share_a_token() {
        // The whole reason the store is keyed per server. If these collided,
        // signing in to one remote server would silently authorize another.
        let secrets = Arc::new(FakeSecretStore::new());
        let first = KeyringCredentialStore::new(secrets.clone(), "example-server");
        let other = KeyringCredentialStore::new(secrets, "other");

        first.save(creds()).await.unwrap();
        assert!(
            other.load().await.unwrap().is_none(),
            "a token saved for one server must be invisible to another"
        );
    }

    #[tokio::test]
    async fn clear_removes_the_credentials() {
        let secrets = Arc::new(FakeSecretStore::new());
        let store = KeyringCredentialStore::new(secrets, "example-server");
        store.save(creds()).await.unwrap();
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }

    /// `load()` returning `None` is NOT enough to prove nothing was left
    /// behind: it short-circuits on the main entry, so a refresh token still
    /// sitting in its own chunked entry would pass the test above while
    /// remaining in the OS credential store.
    ///
    /// That distinction is the whole point of `remove_mcp_remote_server`
    /// clearing credentials — "I removed that server" has to mean the
    /// long-lived refresh token is gone, not merely that the app stopped
    /// looking at it. So this asserts against the store itself.
    #[tokio::test]
    async fn clear_leaves_no_chunk_of_either_token_behind() {
        let secrets = Arc::new(FakeSecretStore::new());
        let store =
            KeyringCredentialStore::new(secrets.clone() as Arc<dyn SecretStore>, "example-server");
        // Deliberately NOT the shared `creds()`: that fixture carries no
        // refresh token, which would make every assertion below pass
        // vacuously — there would be no refresh entry to leak. This one is
        // the realistic Cognito response, whose refresh token is long enough
        // to span several chunks.
        store.save(realistic_cognito_creds()).await.unwrap();

        // The refresh half really is stored separately, so the assertions
        // below are testing something that exists rather than passing
        // vacuously.
        assert!(
            secrets
                .get("mcp-oauth.example-server.refresh.count")
                .is_some(),
            "precondition: the refresh token should have its own entry"
        );

        store.clear().await.unwrap();

        for account in [
            "mcp-oauth.example-server.count",
            "mcp-oauth.example-server.0",
            "mcp-oauth.example-server.refresh.count",
            "mcp-oauth.example-server.refresh.0",
        ] {
            assert!(
                secrets.get(account).is_none(),
                "{account} survived clear() — a credential the user believes \
                 they removed is still in the keystore"
            );
        }
    }

    #[tokio::test]
    async fn an_unreadable_keychain_reads_as_not_signed_in_rather_than_an_error() {
        // Matches OsKeyring's existing error-to-None posture: a broken keystore
        // degrades to "sign in again", it never crashes the app. The spec's
        // error table requires this.
        let store = KeyringCredentialStore::new(Arc::new(FailingSecretStore), "example-server");
        assert!(store.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stored_json_does_not_appear_in_debug_output() {
        let store = KeyringCredentialStore::new(Arc::new(FakeSecretStore::new()), "example-server");
        let printed = format!("{store:?}");
        assert!(!printed.contains("token"), "got {printed}");
    }

    /// A `SecretStore` that enforces the real limit `keyring`'s Windows
    /// backend hits in production. `CRED_MAX_CREDENTIAL_BLOB_SIZE` is 2560
    /// bytes, and the crate doubles a value's UTF-16 code-unit count before
    /// comparing against it — so the real usable ceiling is half that, 1280
    /// units, despite its error text calling the raw 2560 figure "chars"
    /// (confirmed live: a 1259-unit blob saved fine, a 1660-unit one failed
    /// with that exact mislabeled message). Everything else behaves like
    /// `FakeSecretStore`; this exists purely to make the real platform
    /// ceiling reproducible in a test that runs on every OS.
    struct SizeLimitedSecretStore {
        inner: FakeSecretStore,
    }

    impl SizeLimitedSecretStore {
        const WINDOWS_CRED_BLOB_LIMIT_BYTES: usize = 2560;

        fn new() -> Self {
            Self {
                inner: FakeSecretStore::new(),
            }
        }
    }

    impl SecretStore for SizeLimitedSecretStore {
        fn get(&self, account: &str) -> Option<String> {
            self.inner.get(account)
        }

        fn set(&self, account: &str, value: &str) -> Result<(), SecretStoreError> {
            if value.encode_utf16().count() * 2 > Self::WINDOWS_CRED_BLOB_LIMIT_BYTES {
                return Err(SecretStoreError::Backend(format!(
                    "Attribute 'password encoded as UTF-16' is longer than platform limit of {} chars",
                    Self::WINDOWS_CRED_BLOB_LIMIT_BYTES
                )));
            }
            self.inner.set(account, value)
        }

        fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
            self.inner.delete(account)
        }
    }

    /// A realistic Cognito token response: JWT-shaped `access_token`, an
    /// opaque `refresh_token` of the length Cognito actually issues (long
    /// enough on its own to exceed the real ~1280-unit limit, exercising
    /// `store_chunked`'s multi-entry path, not just the split from
    /// `access_token`), and an `id_token` in the vendor extra fields
    /// (present whenever scope includes `openid`, which the uia client
    /// always requests).
    fn realistic_cognito_creds() -> StoredCredentials {
        use oauth2::basic::BasicTokenType;
        use oauth2::{AccessToken, RefreshToken as OAuth2RefreshToken};

        // Not real tokens — just matching real Cognito lengths (JWTs run
        // ~800-1100 chars each; Cognito's opaque refresh tokens commonly run
        // to 1600+, past the real per-entry limit on their own) so this test
        // fails the same way the live bug did if the chunking regresses.
        let access_token = "a".repeat(900);
        let refresh_token = "r".repeat(1600);
        let id_token = "i".repeat(950);

        let mut extra = std::collections::HashMap::new();
        extra.insert("id_token".to_string(), serde_json::Value::String(id_token));

        let mut extra_fields = VendorExtraTokenFields::default();
        extra_fields.0 = extra;
        let mut response = oauth2::StandardTokenResponse::new(
            AccessToken::new(access_token),
            BasicTokenType::Bearer,
            extra_fields,
        );
        response.set_refresh_token(Some(OAuth2RefreshToken::new(refresh_token)));
        response.set_expires_in(Some(&std::time::Duration::from_secs(3600)));

        StoredCredentials::new(
            "exampleclientid0123456789a".to_string(),
            Some(response),
            vec![
                "openid".to_string(),
                "email".to_string(),
                "profile".to_string(),
            ],
            Some(1_700_000_000),
        )
        .with_issuer(Some(
            "https://cognito-idp.ap-southeast-2.amazonaws.com/ap-southeast-2_EXAMPLE00".to_string(),
        ))
    }

    #[tokio::test]
    async fn a_realistic_cognito_response_fits_the_windows_credential_limit() {
        let store =
            KeyringCredentialStore::new(Arc::new(SizeLimitedSecretStore::new()), "example-server");

        store.save(realistic_cognito_creds()).await.expect(
            "a realistic Cognito token response must fit under Windows's per-credential limit",
        );

        let loaded = store
            .load()
            .await
            .unwrap()
            .expect("credentials were just saved");
        let token_response = loaded.token_response.expect("token response was saved");
        assert_eq!(
            token_response.refresh_token().map(|t| t.secret().len()),
            Some(1600),
            "the refresh token must survive the round trip even split across multiple chunks"
        );
        assert_eq!(token_response.access_token().secret().len(), 900);
    }
}
