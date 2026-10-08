// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! OS-native secret storage. `uia-core` must never depend on this module's
//! backing crate (`keyring`) — see `scripts/check-core-deps.sh` — same rule
//! that keeps `cpal`/`tauri` out of the core crate.

use std::collections::HashMap;
use std::sync::Mutex;

/// The canonical account ids. The AWS pair is spelled `aws_*` rather than
/// after the engine, matching AWS's own universal convention — the same names
/// `BedrockSection::resolve_credentials` already falls back to as environment
/// variables (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`).
pub const KNOWN_ACCOUNTS: &[&str] = &[
    "openai_api_key",
    "aws_access_key_id",
    "aws_secret_access_key",
    "foundry_api_key",
];

/// Account ids written before the Bedrock rename. These are still accepted so
/// a keyring entry a user created under the old name stays readable and
/// deletable; a secret in an OS keychain is not something the app can migrate
/// behind their back, and refusing the id outright would make their stored AWS
/// credentials unreachable from the UI that wrote them.
pub const LEGACY_ACCOUNTS: &[&str] = &["nova_access_key_id", "nova_secret_access_key"];

/// The legacy id for a canonical one, where a fallback read makes sense.
pub fn legacy_alias_for(account: &str) -> Option<&'static str> {
    match account {
        "aws_access_key_id" => Some("nova_access_key_id"),
        "aws_secret_access_key" => Some("nova_secret_access_key"),
        _ => None,
    }
}

/// Prefix for per-remote-MCP-server OAuth credential accounts. `oauth_store`'s
/// `KeyringCredentialStore` keys each server's tokens as `mcp-oauth.<server>`
/// so two remote servers never share a token; this prefix is what lets that
/// family of account names pass `validate_account` without enumerating every
/// server name up front.
pub const MCP_OAUTH_ACCOUNT_PREFIX: &str = "mcp-oauth.";

/// True for a per-server OAuth credential account: `mcp-oauth.<server>` where
/// `<server>` passes `uia_mcp::validate_server_name` - the same
/// non-empty, `[A-Za-z0-9_-]`-only rule `mcp_registry.rs` enforces on every
/// server name before it reaches disk. Reusing that function instead of
/// duplicating its character class keeps this the single definition of a
/// legal server name; without it, this account-id check would accept a
/// suffix (arbitrary length, arbitrary characters, including `/` or `..`)
/// straight from frontend-controlled Tauri IPC input that the registry path
/// would have rejected outright.
pub fn is_mcp_oauth_account(account: &str) -> bool {
    account
        .strip_prefix(MCP_OAUTH_ACCOUNT_PREFIX)
        .is_some_and(|server| uia_mcp::validate_server_name(server).is_ok())
}

/// Prefix for per-local-server setting accounts: `mcp-config.<server>.<key>`,
/// the home of a `.mcpb` manifest's `sensitive` `user_config` values.
pub const MCP_CONFIG_ACCOUNT_PREFIX: &str = "mcp-config.";

/// The account for one sensitive setting. Both parts pass the same
/// `[A-Za-z0-9_-]` rule as a server name, so neither can contain the `.`
/// separator and frontend input cannot name an arbitrary account.
pub fn mcp_config_account(server: &str, key: &str) -> Result<String, SecretStoreError> {
    let bad = |what: &str| SecretStoreError::UnknownAccount(format!("{what} is not a valid name"));
    uia_mcp::validate_server_name(server).map_err(|_| bad("server"))?;
    uia_mcp::validate_server_name(key).map_err(|_| bad("setting key"))?;
    Ok(format!("{MCP_CONFIG_ACCOUNT_PREFIX}{server}.{key}"))
}

pub fn is_mcp_config_account(account: &str) -> bool {
    account
        .strip_prefix(MCP_CONFIG_ACCOUNT_PREFIX)
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(server, key)| mcp_config_account(server, key).is_ok())
}

pub fn is_known_account(account: &str) -> bool {
    KNOWN_ACCOUNTS.contains(&account)
        || LEGACY_ACCOUNTS.contains(&account)
        || is_mcp_oauth_account(account)
        || is_mcp_config_account(account)
}

/// The one piece of the Tauri command surface worth unit-testing directly -
/// everything else in `main.rs` is `tauri::command` glue that this sandbox
/// cannot compile (`desktop` feature needs a GTK/WebKit toolchain), matching
/// `set_engine`/`get_engine`'s existing pattern of keeping decision logic out
/// of main.rs.
pub fn validate_account(account: &str) -> Result<(), SecretStoreError> {
    if is_known_account(account) {
        Ok(())
    } else {
        Err(SecretStoreError::UnknownAccount(account.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretStoreError {
    #[error("secret store unavailable: {0}")]
    Backend(String),
    #[error("unknown secret account: {0}")]
    UnknownAccount(String),
}

pub trait SecretStore: Send + Sync {
    /// `None` for an absent entry AND for a keystore that cannot answer, which
    /// is what the API-key fallback chain wants. Anything that must tell the
    /// two apart uses `try_get`.
    fn get(&self, account: &str) -> Option<String>;
    /// Like `get`, but a keystore failure is an error, not "absent".
    fn try_get(&self, account: &str) -> Result<Option<String>, SecretStoreError> {
        Ok(self.get(account))
    }
    fn set(&self, account: &str, value: &str) -> Result<(), SecretStoreError>;
    fn delete(&self, account: &str) -> Result<(), SecretStoreError>;
}

/// Service name every `keyring::Entry` is created under.
const SERVICE: &str = "uia";

pub struct OsKeyring;

impl SecretStore for OsKeyring {
    fn get(&self, account: &str) -> Option<String> {
        let entry = keyring::Entry::new(SERVICE, account).ok()?;
        // A broken/unavailable OS keystore must degrade to the fallback
        // chain, not crash the app - so any error here becomes `None`.
        entry.get_password().ok()
    }

    fn try_get(&self, account: &str) -> Result<Option<String>, SecretStoreError> {
        let entry = keyring::Entry::new(SERVICE, account)
            .map_err(|e| SecretStoreError::Backend(e.to_string()))?;
        match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretStoreError::Backend(e.to_string())),
        }
    }

    fn set(&self, account: &str, value: &str) -> Result<(), SecretStoreError> {
        let entry = keyring::Entry::new(SERVICE, account)
            .map_err(|e| SecretStoreError::Backend(e.to_string()))?;
        entry
            .set_password(value)
            .map_err(|e| SecretStoreError::Backend(e.to_string()))
    }

    fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        let entry = keyring::Entry::new(SERVICE, account)
            .map_err(|e| SecretStoreError::Backend(e.to_string()))?;
        entry
            .delete_credential()
            .map_err(|e| SecretStoreError::Backend(e.to_string()))
    }
}

/// In-memory store for tests - `resolve_api_key`/`resolve_credentials`
/// (config.rs) and the Tauri command tests all use this instead of touching
/// a real OS keychain, keeping `cargo test --workspace` offline.
pub struct FakeSecretStore {
    values: Mutex<HashMap<String, String>>,
}

impl FakeSecretStore {
    pub fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for FakeSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for FakeSecretStore {
    fn get(&self, account: &str) -> Option<String> {
        self.values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .cloned()
    }

    fn set(&self, account: &str, value: &str) -> Result<(), SecretStoreError> {
        self.values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        self.values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
        Ok(())
    }
}

/// A keystore that is present but cannot be read, like a locked Keychain or an
/// absent Secret Service. `get` degrades to `None`; `try_get` reports it.
#[cfg(test)]
pub struct UnreadableSecretStore;

#[cfg(test)]
impl SecretStore for UnreadableSecretStore {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
    fn try_get(&self, _: &str) -> Result<Option<String>, SecretStoreError> {
        Err(SecretStoreError::Backend("keychain is locked".into()))
    }
    fn set(&self, _: &str, _: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Backend("keychain is locked".into()))
    }
    fn delete(&self, _: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Backend("keychain is locked".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_account_returns_none() {
        let store = FakeSecretStore::new();
        assert_eq!(store.get("openai_api_key"), None);
    }

    #[test]
    fn set_then_get_round_trips() {
        let store = FakeSecretStore::new();
        store.set("openai_api_key", "sk-test").unwrap();
        assert_eq!(store.get("openai_api_key"), Some("sk-test".to_string()));
    }

    #[test]
    fn delete_removes_the_value() {
        let store = FakeSecretStore::new();
        store.set("openai_api_key", "sk-test").unwrap();
        store.delete("openai_api_key").unwrap();
        assert_eq!(store.get("openai_api_key"), None);
    }

    #[test]
    fn known_accounts_are_recognised() {
        assert!(is_known_account("openai_api_key"));
        assert!(is_known_account("nova_access_key_id"));
        assert!(is_known_account("nova_secret_access_key"));
        assert!(is_known_account("foundry_api_key"));
        assert!(!is_known_account("something_else"));
    }

    #[test]
    fn validate_account_accepts_known_accounts() {
        assert!(validate_account("openai_api_key").is_ok());
    }

    #[test]
    fn mcp_oauth_accounts_are_recognised_per_server() {
        assert!(is_known_account("mcp-oauth.example-server"));
        assert!(is_known_account("mcp-oauth.other-server"));
        assert!(validate_account("mcp-oauth.example-server").is_ok());
        assert!(!is_known_account("mcp-oauth."));
        assert!(!is_known_account("mcp-oauth"));
    }

    #[test]
    fn mcp_oauth_accounts_reject_a_suffix_outside_the_server_name_charset() {
        // Same charset `uia_mcp::validate_server_name` enforces on every MCP
        // server name written through `mcp_registry.rs` - a suffix arriving
        // straight from Tauri IPC input must not get a looser rule.
        assert!(!is_known_account("mcp-oauth./etc/passwd"));
        assert!(!is_known_account("mcp-oauth.../../secrets"));
        assert!(!is_known_account("mcp-oauth.has space"));
        assert!(validate_account("mcp-oauth./etc/passwd").is_err());
    }

    #[test]
    fn a_server_setting_account_is_known_and_round_trips() {
        let acct = mcp_config_account("mymy-assistant", "api_token").unwrap();
        assert_eq!(acct, "mcp-config.mymy-assistant.api_token");
        assert!(is_known_account(&acct));
        assert!(validate_account(&acct).is_ok());
    }

    #[test]
    fn traversal_shaped_or_empty_parts_are_not_accounts() {
        assert!(mcp_config_account("a/b", "k").is_err());
        assert!(mcp_config_account("srv", "../k").is_err());
        assert!(mcp_config_account("srv", "").is_err());
        assert!(mcp_config_account("", "k").is_err());
        assert!(!is_known_account("mcp-config."));
        assert!(!is_known_account("mcp-config.srv"));
        assert!(!is_known_account("mcp-config.srv.a.b"));
        assert!(!is_known_account("mcp-config.srv.a b"));
    }

    #[test]
    fn validate_account_rejects_unknown_accounts() {
        let err = validate_account("something_else").unwrap_err();
        assert!(err.to_string().contains("something_else"));
    }

    #[test]
    #[ignore = "live: touches the real OS credential store"]
    fn os_keyring_round_trips_a_real_value() {
        let store = OsKeyring;
        let account = "uia_sp2_plan_test_probe";
        store.set(account, "probe-value").unwrap();
        assert_eq!(store.get(account), Some("probe-value".to_string()));
        store.delete(account).unwrap();
        assert_eq!(store.get(account), None);
    }
}
