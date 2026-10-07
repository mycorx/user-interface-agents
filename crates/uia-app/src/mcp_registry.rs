// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! What MCP servers this install is allowed to use.
//!
//! One JSON sidecar (`uia-mcp.json`), same convention as `settings.rs`'s
//! six: next to `uia.toml`, derived by a `*_path_for` function, written
//! only by the Settings UI and read once at startup.
//!
//! It replaces `uia.toml`'s old `[[mcp.servers]]`, which let a server be
//! any command or any URL with no install step and no record of what had
//! been approved. Here a local server must arrive as a validated `.mcpb`
//! bundle, and a remote one must have declared itself over a live MCP
//! handshake before it could be written at all.
//!
//! Deliberately free of `tauri`: `uia-app`'s tests compile with the
//! `desktop` feature OFF, so anything that must be tested cannot live in
//! `main.rs`. The `#[tauri::command]` wrappers there are glue over this.

use crate::secrets::{SecretStore, mcp_config_account};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uia_mcp::McpServerConfig;

/// `uia-mcp.json`, next to `uia.toml` — same sibling convention as
/// `settings::settings_path_for`.
pub fn mcp_registry_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uia-mcp.json")
}

/// Where installed `.mcpb` bundles are extracted to, one directory per
/// local server name. A sibling of the registry that lists them, so a user who
/// moves their config directory takes both.
pub fn mcp_local_servers_dir_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("mcp-local-servers")
}

/// One tool as the server described itself at add time.
///
/// A record of what was approved, shown in Settings — never the list the
/// model is given. That always comes fresh from `McpRouter::list_tools()`
/// at session start, so a snapshot going stale can mislead a human reading
/// Settings but never the assistant's own tool declaration.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct DeclaredTool {
    pub name: String,
    pub description: String,
}

/// A local server installed from a `.mcpb`. The bundle's own files live in
/// `mcp_local_servers_dir_for(..)/<name>/`; this is only the record that it was
/// installed and whether the user has enabled it.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct LocalServerEntry {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    /// Plain (non-sensitive) `user_config` values. Sensitive ones live only in
    /// the OS keyring, never here.
    #[serde(default)]
    pub user_config: BTreeMap<String, String>,
}

/// How a remote server authenticates. `Static` is a pasted header (the
/// original, Claude-Code-`-H` shape); `OAuth` means the server's bearer token
/// comes from an `OAuthProvider` backed by the OS keychain instead.
///
/// Deliberately data-less: the OAuth `client_id` an `OAuth` server needs
/// lives on `RemoteEntry::oauth_client_id` instead, not as a field on this
/// enum (an earlier design put it here — wrong, since the value is per-server
/// configuration, not part of *which* auth mode a server uses).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy, Default)]
pub enum RemoteAuth {
    #[default]
    #[serde(rename = "static")]
    Static,
    #[serde(rename = "oauth")]
    OAuth,
}

/// A remote HTTP server the user added by hand, together with the
/// declaration it gave when they added it.
#[derive(Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct RemoteEntry {
    pub name: String,
    pub url: String,
    /// The optional single auth header, Claude Code's `-H`. The value is a
    /// secret: never logged, never returned to the frontend, never quoted
    /// into an error message. Only meaningful when `auth` is `Static` —
    /// `preview_remote` refuses an `OAuth` entry that also sets this (Task
    /// 2's collision guard, applied here too).
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub header_value: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub declared_name: Option<String>,
    #[serde(default)]
    pub declared_version: Option<String>,
    #[serde(default)]
    pub tools: Vec<DeclaredTool>,
    /// `#[serde(default)]` so a registry saved before this field existed
    /// still loads, as `Static` — every server on disk today authenticates
    /// with a pasted header or none at all.
    #[serde(default)]
    pub auth: RemoteAuth,
    /// The public OAuth client id for the PKCE flow. Not a secret — PKCE
    /// has no client secret — so unlike `header_value` it needs no
    /// redaction. Populated only when `auth == RemoteAuth::OAuth`.
    #[serde(default)]
    pub oauth_client_id: Option<String>,
}

/// The placeholder a set header value is printed as — the same literal
/// `client.rs` uses for `McpServerConfig::Http`'s `Debug`, so the two read
/// consistently wherever a header value shows up in debug output.
const REDACTED: &str = "<redacted>";

/// Hand-written rather than derived, ON PURPOSE — do not "tidy" this back
/// into `#[derive(Debug)]`.
///
/// `header_value` holds a bearer token, and a derived `Debug` would print it
/// in full — not just from a direct `{:?}` on a `RemoteEntry`, but from
/// anything that merely *contains* one, `McpRegistry` included, since a
/// derived `Debug` delegates to each field's own impl. The spec is that a
/// remote server's header value is never logged and never round-tripped
/// into an error message; this impl is what makes that true even for a
/// caller who never thought about it.
///
/// Every other field prints in full — they are not secret and are what a
/// debugger actually needs. `header_value` prints as `Some(<redacted>)` or
/// `None` so a debugger can still tell whether a header is set at all,
/// without ever seeing what it is.
impl std::fmt::Debug for RemoteEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEntry")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("header_name", &self.header_name)
            .field(
                "header_value",
                &self.header_value.as_ref().map(|_| REDACTED),
            )
            .field("enabled", &self.enabled)
            .field("declared_name", &self.declared_name)
            .field("declared_version", &self.declared_version)
            .field("tools", &self.tools)
            .field("auth", &self.auth)
            .field("oauth_client_id", &self.oauth_client_id)
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct McpRegistry {
    #[serde(default)]
    pub local_servers: Vec<LocalServerEntry>,
    #[serde(default)]
    pub remote_servers: Vec<RemoteEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry not writable: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("no local server named {0:?} is installed")]
    NoSuchLocalServer(String),
    #[error("no remote server named {0:?} has been added")]
    NoSuchRemote(String),
    #[error(
        "an MCP server named {0:?} already exists; the name is the namespace every one of \
         its tools is routed by, so two servers cannot share one"
    )]
    DuplicateName(String),
    #[error("{0}")]
    Name(String),
    #[error(
        "{0:?} is not an http:// or https:// URL with a host; a bare hostname or a pasted \
         value with a stray space is the usual cause"
    )]
    InvalidUrl(String),
    #[error(
        "nothing at that URL answered the MCP initialize handshake: {0}. A remote server \
         has no bundle to inspect, so its own declaration is the only thing there is to \
         review \u{2014} without one there is nothing to trust"
    )]
    Unreachable(String),
    #[error("that server connected but declared no tools, so there is no capability to approve")]
    NoDeclaredTools,
    #[error(
        "this server authenticates with OAuth, so it must not also carry a static \
         Authorization header - remove the header or switch the server to static-header auth"
    )]
    OAuthHeaderConflict,
    /// The server answered 401 and needs a sign-in round trip before it can be
    /// added. `state` is carried for the caller (it is the key `preview_remote`
    /// stashed the in-flight `AuthorizationSession` under, and what the
    /// eventual browser callback will report back), but deliberately never
    /// appears in `Display`: it is a CSRF token, and an error message is
    /// exactly the kind of text a user might paste into a bug report or a
    /// support channel.
    #[error("this server requires you to sign in before it can be added")]
    NeedsAuthorization {
        authorize_url: String,
        state: String,
    },
}

/// The default registry when the file is missing or unreadable — the same
/// tolerance `settings::load` has, and for the same reason: a corrupt
/// UI-written sidecar must never block startup. The cost of being wrong is
/// that servers do not load, which is the safe direction.
pub fn load(path: &Path) -> McpRegistry {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, registry: &McpRegistry) -> Result<(), RegistryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(registry)?)?;
    Ok(())
}

/// What a local server will actually run, resolved fresh from disk.
///
/// `env_keys` rather than the environment itself: the values come out of the
/// manifest's template substitution and can carry paths or tokens, so they
/// get the same treatment as a remote's header value — the frontend learns
/// that a variable is set, never what it is set to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocalServerDetails {
    pub version: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
}

/// Resolves what `name` would launch, using the same `parse_manifest` +
/// `validate_bundle` path `session::mcp_targets` runs at every startup.
///
/// Deliberately resolved on demand rather than stored on `LocalServerEntry`:
/// the registry records what was *approved*, while the launch command is
/// derived from a `manifest.json` that can be rewritten underneath us — which
/// is exactly why the validation re-runs at every launch instead of trusting
/// the install. A stored copy could show one command while a different one
/// ran; resolving here means the panel shows what will actually run.
///
/// The `Err` case is worth as much as the `Ok` one: it is the same failure
/// `mcp_targets` would hit at the next launch, surfaced in the UI now instead
/// of only reaching stderr after a restart. Its wording deliberately matches
/// `mcp_targets`'s skip reasons, so the Details panel and the Status column
/// describe one problem in one voice.
///
/// Lives here rather than inside `main.rs`'s `#[tauri::command]` because none
/// of it needs Tauri, and the binary is behind
/// `required-features = ["desktop"]` — anything in there is invisible to
/// `cargo test --workspace` and untestable without a GUI toolchain. The
/// command is a three-line adapter over this.
pub fn describe_local_server(
    registry: &McpRegistry,
    local_servers_dir: &Path,
    name: &str,
) -> Result<LocalServerDetails, String> {
    let entry = registry
        .local_servers
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no local server named {name:?} is installed"))?;

    let dir = local_servers_dir.join(&entry.name);
    let raw = std::fs::read_to_string(dir.join("manifest.json"))
        .map_err(|e| format!("its installed files are missing: {e}"))?;
    let manifest = uia_mcp::bundle::parse_manifest(&raw)
        .map_err(|e| format!("its manifest.json is unreadable: {e}"))?;
    let launch =
        uia_mcp::bundle::validate_bundle(&manifest, uia_mcp::bundle::current_platform(), &dir)
            .map_err(|e| format!("it no longer passes bundle validation: {e}"))?;

    Ok(LocalServerDetails {
        version: entry.version.clone(),
        command: launch.command,
        args: launch.args,
        env_keys: launch.env.into_iter().map(|(k, _)| k).collect(),
    })
}

/// One field of a server's settings form. A sensitive field never carries its
/// value — only whether one is stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigFieldView {
    pub key: String,
    pub kind: String,
    pub title: String,
    pub description: Option<String>,
    pub required: bool,
    pub sensitive: bool,
    pub multiple: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub default: Option<String>,
    pub value: Option<String>,
    pub is_set: bool,
}

type Fields = BTreeMap<String, uia_mcp::bundle::UserConfigField>;

fn read_fields(local_servers_dir: &Path, name: &str) -> Result<Fields, String> {
    let raw = std::fs::read_to_string(local_servers_dir.join(name).join("manifest.json"))
        .map_err(|e| format!("its installed files are missing: {e}"))?;
    uia_mcp::bundle::parse_manifest(&raw)
        .map(|m| m.user_config)
        .map_err(|e| format!("its manifest.json is unreadable: {e}"))
}

/// What `apply_user_config` will be given: plain values from the registry
/// entry, sensitive ones from the keyring. A keyring that cannot answer reads
/// as "unset", which `apply_user_config` then reports for a required field.
pub fn local_config_values(
    entry: &LocalServerEntry,
    fields: &Fields,
    secrets: &dyn SecretStore,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, field) in fields {
        let stored = if field.sensitive {
            mcp_config_account(&entry.name, key)
                .ok()
                .and_then(|a| secrets.get(&a))
        } else {
            entry.user_config.get(key).cloned()
        };
        if let Some(v) = stored.filter(|v| !v.is_empty()) {
            out.insert(key.clone(), v);
        }
    }
    out
}

/// The MCPB path variables for this machine, for `expand_path_vars`.
///
/// A directory the OS cannot name is omitted rather than guessed, so the
/// variable stays as written in the text instead of becoming a wrong path.
/// The lookup lives here, not in `uia-mcp`, which has no `directories`.
pub fn mcp_path_vars() -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    if let Some(dirs) = directories::UserDirs::new() {
        let named = [
            ("HOME", Some(dirs.home_dir())),
            ("DESKTOP", dirs.desktop_dir()),
            ("DOCUMENTS", dirs.document_dir()),
            ("DOWNLOADS", dirs.download_dir()),
        ];
        for (name, dir) in named {
            if let Some(dir) = dir {
                vars.insert(name.to_string(), dir.to_string_lossy().into_owned());
            }
        }
    }
    for name in ["/", "pathSeparator"] {
        vars.insert(name.to_string(), std::path::MAIN_SEPARATOR_STR.to_string());
    }
    vars
}

pub fn describe_local_config(
    registry: &McpRegistry,
    local_servers_dir: &Path,
    secrets: &dyn SecretStore,
    name: &str,
) -> Result<Vec<ConfigFieldView>, String> {
    let entry = registry
        .local_servers
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no local server named {name:?} is installed"))?;
    let fields = read_fields(local_servers_dir, name)?;
    let values = local_config_values(entry, &fields, secrets);
    let vars = mcp_path_vars();
    Ok(fields
        .into_iter()
        .map(|(key, f)| {
            let stored = values.get(&key).cloned();
            ConfigFieldView {
                title: f.title.clone().unwrap_or_else(|| key.clone()),
                description: f.description.clone(),
                required: f.required,
                sensitive: f.sensitive,
                multiple: f.multiple,
                min: f.min,
                max: f.max,
                // What the server will actually get, so the form shows the real
                // path rather than `${HOME}/...`.
                default: f
                    .default_text()
                    .map(|d| uia_mcp::bundle::expand_path_vars(&d, &vars)),
                is_set: stored.is_some(),
                value: if f.sensitive { None } else { stored },
                kind: if f.kind.is_empty() {
                    "string".into()
                } else {
                    f.kind
                },
                key,
            }
        })
        .collect())
}

fn check_value(key: &str, f: &uia_mcp::bundle::UserConfigField, v: &str) -> Result<(), String> {
    match f.kind.as_str() {
        "number" => {
            let n: f64 = v
                .trim()
                .parse()
                .map_err(|_| format!("{key}: {v:?} is not a number"))?;
            if !n.is_finite() {
                return Err(format!("{key}: {v:?} is not a finite number"));
            }
            if f.min.is_some_and(|m| n < m) || f.max.is_some_and(|m| n > m) {
                return Err(format!("{key}: {n} is outside the allowed range"));
            }
            Ok(())
        }
        "boolean" if v != "true" && v != "false" => {
            Err(format!("{key}: expected true or false, got {v:?}"))
        }
        _ => Ok(()),
    }
}

/// Validates every change against the manifest, then writes: keyring first,
/// registry second, so a keyring failure leaves the registry untouched.
pub fn save_local_config(
    registry_path: &Path,
    local_servers_dir: &Path,
    secrets: &dyn SecretStore,
    name: &str,
    changes: BTreeMap<String, Option<String>>,
) -> Result<(), String> {
    let mut registry = load(registry_path);
    let entry = registry
        .local_servers
        .iter_mut()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no local server named {name:?} is installed"))?;
    let fields = read_fields(local_servers_dir, name)?;

    for (key, change) in &changes {
        let f = fields
            .get(key)
            .ok_or_else(|| format!("{key}: this server declares no such setting"))?;
        if let Some(v) = change.as_deref().filter(|v| !v.is_empty()) {
            check_value(key, f, v)?;
        }
    }

    // What the settings would be after this save, to enforce `required`.
    let mut after = local_config_values(entry, &fields, secrets);
    for (key, change) in &changes {
        match change.as_deref().filter(|v| !v.is_empty()) {
            Some(v) => {
                after.insert(key.clone(), v.to_string());
            }
            None => {
                after.remove(key);
            }
        }
    }
    for (key, f) in &fields {
        if f.required && !after.contains_key(key) && f.default_text().is_none() {
            return Err(format!("{key}: this setting is required"));
        }
    }

    // Snapshot every sensitive key this save touches, so a failure part-way
    // can put the keyring back exactly as it was.
    let mut snapshot: Vec<(String, Option<String>)> = Vec::new();
    let mut keyring_ops: Vec<(String, Option<String>)> = Vec::new();
    for (key, change) in &changes {
        if !fields[key].sensitive {
            continue;
        }
        let account = mcp_config_account(name, key).map_err(|e| e.to_string())?;
        snapshot.push((account.clone(), secrets.get(&account)));
        keyring_ops.push((account, change.clone().filter(|v| !v.is_empty())));
    }

    let rollback = |why: String| -> String {
        let mut failed = Vec::new();
        for (account, prior) in &snapshot {
            let restored = match prior {
                Some(v) => secrets.set(account, v),
                // Absent is already the desired state; deleting an absent
                // entry is an error on the real keyring.
                None if secrets.get(account).is_some() => secrets.delete(account),
                None => Ok(()),
            };
            if let Err(e) = restored {
                failed.push(format!("{account}: {e}"));
            }
        }
        if failed.is_empty() {
            why
        } else {
            format!(
                "{why} (and restoring the previous keyring values failed: {})",
                failed.join("; ")
            )
        }
    };

    for ((account, new), (_, prior)) in keyring_ops.iter().zip(&snapshot) {
        let applied = match new {
            Some(v) => secrets.set(account, v),
            None if prior.is_some() => secrets.delete(account),
            None => Ok(()),
        };
        if let Err(e) = applied {
            return Err(rollback(e.to_string()));
        }
    }
    for (key, change) in &changes {
        if fields[key].sensitive {
            continue;
        }
        match change.as_deref().filter(|v| !v.is_empty()) {
            Some(v) => {
                entry.user_config.insert(key.clone(), v.to_string());
            }
            None => {
                entry.user_config.remove(key);
            }
        }
    }
    save(registry_path, &registry).map_err(|e| rollback(e.to_string()))
}

/// Record first, credentials second, files last — the order
/// `remove_remote_and_clear_credentials` uses, so a failure part-way cannot
/// resurrect a server the user removed. The manifest is read up front: it is
/// the only record of which keys have keyring entries.
pub fn remove_local_server_and_clear_config(
    registry_path: &Path,
    local_servers_dir: &Path,
    secrets: &dyn SecretStore,
    name: &str,
) -> Result<(), String> {
    let sensitive_keys: Vec<String> = read_fields(local_servers_dir, name)
        .map(|f| {
            f.into_iter()
                .filter(|(_, v)| v.sensitive)
                .map(|(k, _)| k)
                .collect()
        })
        .unwrap_or_default();
    let mut registry = load(registry_path);
    registry
        .remove_local_server(name)
        .map_err(|e| e.to_string())?;
    save(registry_path, &registry).map_err(|e| e.to_string())?;
    for key in sensitive_keys {
        if let Ok(account) = mcp_config_account(name, &key) {
            let _ = secrets.delete(&account);
        }
    }
    std::fs::remove_dir_all(local_servers_dir.join(name)).ok();
    Ok(())
}

/// Removes a remote server and deletes any saved OAuth sign-in with it.
///
/// The two halves belong together. Removing only the record left the access
/// AND refresh tokens in the OS credential store permanently, and unreachable
/// with it: the account key is derived from a server name that is no longer
/// in the registry, so nothing could name them again. A refresh token is
/// long-lived, so "I removed that server" has to mean the credential is gone,
/// not merely orphaned.
///
/// Order matters. The record goes first, for the same reason
/// `remove_mcp_local_server` deletes its files in that order: a failure
/// clearing credentials must not resurrect a server the user has removed. The
/// reverse would be worse still — a server the UI keeps listing whose
/// credentials were already destroyed.
///
/// `secrets` is injected rather than reaching for `OsKeyring` directly, which
/// is what lets this be tested at all; the command in `main.rs` supplies the
/// real one.
pub async fn remove_remote_and_clear_credentials(
    registry_path: &Path,
    name: &str,
    secrets: Arc<dyn crate::secrets::SecretStore>,
) -> Result<(), String> {
    use uia_mcp::auth::CredentialStore as _;

    let mut registry = load(registry_path);
    // Read before the entry is dropped: it is the only thing that says whether
    // there is a saved sign-in to delete. A static-header server has no
    // keychain entry at all, and `clear` on one would fail deleting a
    // credential that was never supposed to exist.
    let was_oauth = registry
        .remote_servers
        .iter()
        .any(|r| r.name == name && r.auth == RemoteAuth::OAuth);

    registry.remove_remote(name).map_err(|e| e.to_string())?;
    save(registry_path, &registry).map_err(|e| e.to_string())?;

    if !was_oauth {
        return Ok(());
    }

    crate::oauth_store::KeyringCredentialStore::new(secrets, name)
        .clear()
        .await
        .map_err(|e| {
            format!(
                "{name:?} was removed, but deleting its saved sign-in failed, so the \
                 token is still in your credential store: {e}"
            )
        })
}

impl McpRegistry {
    /// Every server name in use, local and remote alike. One namespace:
    /// `McpRouter` dispatches a `server.tool` call on the `server` half
    /// alone, so a remote shadowing a local server would silently route to the
    /// wrong one.
    fn name_taken(&self, name: &str) -> bool {
        self.local_servers.iter().any(|p| p.name == name)
            || self.remote_servers.iter().any(|r| r.name == name)
    }

    pub fn set_local_server_enabled(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> Result<(), RegistryError> {
        let entry = self
            .local_servers
            .iter_mut()
            .find(|p| p.name == name)
            .ok_or_else(|| RegistryError::NoSuchLocalServer(name.to_string()))?;
        entry.enabled = enabled;
        Ok(())
    }

    /// Drops the record only. Deleting the extracted files is the caller's
    /// job, because it is the caller that knows the local servers directory —
    /// see `main.rs`'s `remove_mcp_local_server`.
    pub fn remove_local_server(&mut self, name: &str) -> Result<(), RegistryError> {
        let before = self.local_servers.len();
        self.local_servers.retain(|p| p.name != name);
        if self.local_servers.len() == before {
            return Err(RegistryError::NoSuchLocalServer(name.to_string()));
        }
        Ok(())
    }

    /// Records an installed bundle. Always disabled: installing states
    /// what a local server is, enabling states that it is trusted, and folding
    /// the two together would make the second one invisible.
    pub fn add_local_server(
        &mut self,
        name: String,
        version: Option<String>,
    ) -> Result<(), RegistryError> {
        if self.name_taken(&name) {
            return Err(RegistryError::DuplicateName(name));
        }
        self.local_servers.push(LocalServerEntry {
            name,
            version,
            enabled: false,
            user_config: BTreeMap::new(),
        });
        Ok(())
    }

    /// Records a remote server that has already declared itself. `enabled`
    /// on the incoming entry is ignored — see `add_local_server`.
    pub fn add_remote(&mut self, mut entry: RemoteEntry) -> Result<(), RegistryError> {
        uia_mcp::validate_server_name(&entry.name)
            .map_err(|e| RegistryError::Name(e.to_string()))?;
        if self.name_taken(&entry.name) {
            return Err(RegistryError::DuplicateName(entry.name));
        }
        entry.enabled = false;
        self.remote_servers.push(entry);
        Ok(())
    }

    pub fn set_remote_enabled(&mut self, name: &str, enabled: bool) -> Result<(), RegistryError> {
        let entry = self
            .remote_servers
            .iter_mut()
            .find(|r| r.name == name)
            .ok_or_else(|| RegistryError::NoSuchRemote(name.to_string()))?;
        entry.enabled = enabled;
        Ok(())
    }

    pub fn remove_remote(&mut self, name: &str) -> Result<(), RegistryError> {
        let before = self.remote_servers.len();
        self.remote_servers.retain(|r| r.name != name);
        if self.remote_servers.len() == before {
            return Err(RegistryError::NoSuchRemote(name.to_string()));
        }
        Ok(())
    }
}

impl RemoteEntry {
    /// The `(name, header)` pairs this entry contributes to a request.
    pub fn headers(&self) -> Vec<(String, String)> {
        match (&self.header_name, &self.header_value) {
            (Some(n), Some(v)) if !n.is_empty() => vec![(n.clone(), v.clone())],
            _ => Vec::new(),
        }
    }

    pub fn transport(&self) -> McpServerConfig {
        McpServerConfig::Http {
            url: self.url.clone(),
            headers: self.headers(),
            // Task 2's collision guard lives at `http_transport_config`; this
            // is the flag that trips it. `Static` sends `headers()` and
            // nothing else, same as before this task.
            expects_token: self.auth == RemoteAuth::OAuth,
        }
    }
}

/// Turn a live server's answer into the entry that would be stored.
///
/// Split from `preview_remote` for the same reason `session::mcp_targets`
/// is split from `build_executor`: the mapping is the part worth testing,
/// and testing it must not require a server to connect to.
#[allow(clippy::too_many_arguments)]
pub fn remote_entry_from(
    name: String,
    url: String,
    header_name: Option<String>,
    header_value: Option<String>,
    declaration: Option<uia_mcp::ServerDeclaration>,
    tools: &[uia_core::tools::ToolDescriptor],
) -> Result<RemoteEntry, RegistryError> {
    uia_mcp::validate_server_name(&name).map_err(|e| RegistryError::Name(e.to_string()))?;
    if !crate::settings::is_valid_http_url(&url) {
        return Err(RegistryError::InvalidUrl(url));
    }
    if tools.is_empty() {
        return Err(RegistryError::NoDeclaredTools);
    }
    Ok(RemoteEntry {
        name,
        url,
        header_name,
        header_value,
        enabled: false,
        declared_name: declaration.as_ref().map(|d| d.name.clone()),
        declared_version: declaration.and_then(|d| d.version),
        tools: tools
            .iter()
            .map(|t| DeclaredTool {
                name: t.name.clone(),
                description: t.description.clone(),
            })
            .collect(),
        // `preview_remote` overwrites both after this call for an OAuth
        // entry; every other caller gets the `Static`/`None` defaults that
        // already describe a pasted-header (or no-auth) server.
        auth: RemoteAuth::Static,
        oauth_client_id: None,
    })
}

/// How long `preview_remote` waits for a server to answer `initialize` and
/// `tools/list` before giving up. rmcp's HTTP transport carries its own
/// `ExponentialBackoff` retry policy, so an unresponsive URL would otherwise
/// stall rather than fail — and a Settings dialog must never hang on a URL
/// the user mistyped. Also the timeout for the OAuth connect *legs* (the two
/// machine-to-machine HTTP round trips), NOT for the human's time in the
/// browser — see `main.rs`'s `OAUTH_BROWSER_TIMEOUT` for that one.
const PREVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long `preview_remote` waits for the post-401 OAuth discovery leg —
/// resolving authorization-server metadata from the challenge's own
/// `resource_metadata` pointer, then (when the caller supplied no
/// `oauth_client_id`) Dynamic Client Registration against that same server —
/// before giving up. Distinct from `PREVIEW_TIMEOUT` (which bounds only the
/// initial, unauthenticated connect attempt) and from `main.rs`'s
/// `OAUTH_BROWSER_TIMEOUT` (which bounds a human reading a consent screen,
/// not a machine-to-machine call): this is two more HTTP round trips against
/// an authorization server the initial connect already proved is reachable,
/// so it gets a bound in the same family as `PREVIEW_TIMEOUT`'s 10s rather
/// than `OAUTH_BROWSER_TIMEOUT`'s five minutes — 20s is generous enough for a
/// real (if slow) metadata document and DCR round trip, while still failing
/// a black-holed endpoint well within the Settings dialog staying responsive.
///
/// Shrunk under `#[cfg(test)]` so the regression test proving this bound
/// actually fires (a discovery endpoint that never responds) does not cost
/// 20 real seconds of test time; the constant's *value* is what changes
/// between the two builds, never the logic that consults it.
#[cfg(not(test))]
const OAUTH_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
#[cfg(test)]
const OAUTH_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// One in-flight OAuth sign-in, alongside the identity `preview_remote` was
/// called with — `AuthorizationSession` itself carries no such label, but a
/// retry (the frontend's "check again" button, or simply re-opening the same
/// Settings form) only ever hands `preview_remote` a `name`/`url` again,
/// never the `state` a previous call minted, so looking an existing session
/// up again means searching by the request that would have produced it.
/// `url` is part of the identity, not just `name`: two calls that share a
/// name but differ in `url` (the user edited the form before retrying) are
/// NOT the same pending sign-in and must not be conflated — see
/// `PendingOAuthSessions::take_stale_for` for what happens to the old one
/// in that case.
struct PendingSession {
    name: String,
    url: String,
    session: uia_mcp::auth::AuthorizationSession,
}

/// Where an in-flight `AuthorizationSession` waits between `preview_remote`
/// returning `RegistryError::NeedsAuthorization` and the background task
/// (`main.rs`) that completes the browser round trip. Keyed by the CSRF
/// `state` that error carries.
///
/// Lives here, not as a bare type inline in `main.rs`, because `preview_remote`
/// — the only code that ever holds a session before its callback arrives — is
/// what must stash it, and `main.rs` cannot be a dependency of this module.
/// `main.rs` still owns the *managing*: it `.manage()`s this exact type
/// unwrapped, the same way it already does `oauth_flow::DeepLinkListener`, so
/// this module stays free of `tauri` per this file's own header note. Plain
/// `std::sync` primitives only, matching `uia_mcp::callback::
/// PendingAuthorizations`'s own shape for the same reason: this is a
/// synchronous map lookup guarded by a lock, nothing tokio-flavored.
///
/// Also tracks which states already have a `main.rs`-spawned background task
/// waiting on their browser round trip (`tasks_spawned`) — added for finding
/// 2 of the 2026-08-30 whole-branch review. Before that fix, every retry
/// built a brand new session with a fresh CSRF state, so there was never a
/// question of two background tasks racing over the SAME state. Once a retry
/// resumes the existing session instead (see `existing_for`), `main.rs`'s
/// `complete_oauth_sign_in` would otherwise spawn a second `await_callback`
/// waiter for that same state on every "check again" click — and since
/// `DeepLinkListener::await_callback` keys its oneshot waiters by state too,
/// a second registration replaces (and thereby cancels) the first task's
/// wait rather than running alongside it. `should_spawn_task`/`finish_task`
/// are what let `main.rs` tell "first time" from "already waiting" apart.
#[derive(Default)]
pub struct PendingOAuthSessions(Mutex<PendingOAuthSessionsInner>);

#[derive(Default)]
struct PendingOAuthSessionsInner {
    sessions: HashMap<String, PendingSession>,
    tasks_spawned: std::collections::HashSet<String>,
}

impl PendingOAuthSessions {
    fn insert(
        &self,
        state: String,
        name: String,
        url: String,
        session: uia_mcp::auth::AuthorizationSession,
    ) {
        self.0
            .lock()
            .expect("PendingOAuthSessions mutex poisoned")
            .sessions
            .insert(state, PendingSession { name, url, session });
    }

    /// Removes and returns the session for `state`, so a replayed or
    /// duplicate callback cannot complete the same sign-in twice.
    pub fn take(&self, state: &str) -> Option<uia_mcp::auth::AuthorizationSession> {
        self.0
            .lock()
            .expect("PendingOAuthSessions mutex poisoned")
            .sessions
            .remove(state)
            .map(|p| p.session)
    }

    /// The authorize URL and CSRF state of a sign-in already pending for
    /// this exact `(name, url)`, if one exists — `preview_remote` calls this
    /// first thing in its OAuth branch so a retry before the human finishes
    /// signing in (the frontend's "check again" button, or simply clicking
    /// Connect again) resumes THIS session rather than building a fresh one
    /// with a fresh CSRF state, abandoning the old one and its
    /// `PendingOAuthSessions`/`DeepLinkListener` entries. A linear scan, not
    /// a second `name -> state` index: this map holds one entry per server
    /// the user is mid-sign-in with, in practice a handful at most.
    fn existing_for(&self, name: &str, url: &str) -> Option<(String, String)> {
        self.0
            .lock()
            .expect("PendingOAuthSessions mutex poisoned")
            .sessions
            .iter()
            .find(|(_, p)| p.name == name && p.url == url)
            .map(|(state, p)| (state.clone(), p.session.get_authorization_url().to_string()))
    }

    /// Removes and returns the CSRF state of a session pending for `name`
    /// under a DIFFERENT `url` than the one just requested — the user edited
    /// the form (changed the URL, presumably) and retried rather than
    /// clicking "check again" on the same request. That old session's
    /// `authorize_url` points at the wrong server and can never be resumed,
    /// so rather than let it sit unreachable until `main.rs`'s
    /// `OAUTH_BROWSER_TIMEOUT` eventually reaps its background task five
    /// minutes later, `preview_remote` calls this to drop it immediately and
    /// hands the returned state to `uia_mcp::callback::PendingAuthorizations::
    /// cancel` so its callback routing entry goes with it.
    fn take_stale_for(&self, name: &str, url: &str) -> Option<String> {
        let mut inner = self.0.lock().expect("PendingOAuthSessions mutex poisoned");
        let stale_state = inner
            .sessions
            .iter()
            .find(|(_, p)| p.name == name && p.url != url)
            .map(|(state, _)| state.clone())?;
        inner.sessions.remove(&stale_state);
        inner.tasks_spawned.remove(&stale_state);
        Some(stale_state)
    }

    /// Records that a background task is now waiting on `state`'s browser
    /// round trip, returning `true` the first time (the caller should spawn
    /// one) and `false` on every later call for the same state — a repeated
    /// "check again" click that `existing_for` resumed rather than replaced.
    /// `main.rs`'s `complete_oauth_sign_in` is the only caller.
    pub fn should_spawn_task(&self, state: &str) -> bool {
        self.0
            .lock()
            .expect("PendingOAuthSessions mutex poisoned")
            .tasks_spawned
            .insert(state.to_string())
    }

    /// Releases the bookkeeping `should_spawn_task` set once the background
    /// task for `state` has run to completion (success, failure, or
    /// timeout). Without this, `tasks_spawned` would grow by one entry per
    /// sign-in attempt for the life of the process — the same unbounded leak
    /// finding 2 identifies for the sessions map itself.
    pub fn finish_task(&self, state: &str) {
        self.0
            .lock()
            .expect("PendingOAuthSessions mutex poisoned")
            .tasks_spawned
            .remove(state);
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.0
            .lock()
            .expect("PendingOAuthSessions mutex poisoned")
            .sessions
            .len()
    }
}

/// Connect to a remote server and return the entry its own declaration
/// justifies. Writes nothing: persisting is `add_remote`'s job, so the UI
/// can show what came back and let the user decide.
///
/// `RemoteAuth::OAuth` is a two-phase flow, not a single connect:
///
/// 1. No credentials are stored yet (or the frontend is re-previewing before
///    ever signing in): the connect attempt below goes out with no bearer
///    token, the server answers 401 with a `WWW-Authenticate` challenge, and
///    this function returns `Err(RegistryError::NeedsAuthorization { .. })`
///    with a browser URL for the caller to open. It does NOT block here
///    waiting for the human to finish signing in — that would hang the Tauri
///    command for the whole sign-in duration with no way to hand the
///    frontend the URL first. The in-flight session (holding the PKCE
///    verifier) is stashed in `pending_sessions`, keyed by the CSRF state the
///    error also carries, for a later step (`main.rs`) to resume.
/// 2. Credentials already exist (a retry after the browser round trip
///    completed and `main.rs`'s background task ran `handle_callback_url`,
///    which persists them into the keychain via the manager's credential
///    store): the same connect attempt now succeeds directly, since
///    `OAuthProvider::token()` finds a valid stored token. No second browser
///    flow, no dedicated "finish" command — a plain retry of this same
///    function is what completes the add.
#[allow(clippy::too_many_arguments)]
pub async fn preview_remote(
    name: String,
    url: String,
    header_name: Option<String>,
    header_value: Option<String>,
    oauth_client_id: Option<String>,
    auth: RemoteAuth,
    pending_sessions: &PendingOAuthSessions,
    pending_authorizations: &uia_mcp::callback::PendingAuthorizations,
) -> Result<RemoteEntry, RegistryError> {
    use uia_core::tools::ToolExecutor;

    uia_mcp::validate_server_name(&name).map_err(|e| RegistryError::Name(e.to_string()))?;
    if !crate::settings::is_valid_http_url(&url) {
        return Err(RegistryError::InvalidUrl(url));
    }
    // Task 2's guard lives at `http_transport_config` too (it would catch
    // this at connect time regardless), but checking here as well gives a
    // clearer, OAuth-specific message before any connection is attempted —
    // the two are meant to agree, not race to be first.
    if auth == RemoteAuth::OAuth
        && header_name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case("authorization"))
    {
        return Err(RegistryError::OAuthHeaderConflict);
    }

    match auth {
        RemoteAuth::Static => {
            let probe = RemoteEntry {
                name: name.clone(),
                url: url.clone(),
                header_name: header_name.clone(),
                header_value: header_value.clone(),
                auth: RemoteAuth::Static,
                ..Default::default()
            };

            let name_for_task = name.clone();
            let connect_and_list = async move {
                let executor = uia_mcp::McpExecutor::connect(&name_for_task, probe.transport())
                    .await
                    .map_err(|e| RegistryError::Unreachable(e.to_string()))?;
                let declaration = executor.declaration();
                let tools = executor
                    .list_tools()
                    .await
                    .map_err(|e| RegistryError::Unreachable(e.to_string()))?;
                Ok::<_, RegistryError>((declaration, tools))
            };

            let (declaration, tools) = tokio::time::timeout(PREVIEW_TIMEOUT, connect_and_list)
                .await
                .map_err(|_| {
                    RegistryError::Unreachable("the server did not answer in time".to_string())
                })??;

            remote_entry_from(name, url, header_name, header_value, declaration, &tools)
        }
        RemoteAuth::OAuth => {
            // Finding 2 (2026-08-30 whole-branch review): resume an
            // already-pending sign-in for this exact request rather than
            // unconditionally minting a fresh `AuthorizationManager`/
            // `AuthorizationSession` — see `existing_for`'s doc comment for
            // why a fresh session on every retry was a real, unbounded leak.
            if let Some((state, authorize_url)) = pending_sessions.existing_for(&name, &url) {
                return Err(RegistryError::NeedsAuthorization {
                    authorize_url,
                    state,
                });
            }
            // A pending sign-in exists for this NAME but a different `url`
            // (the form was edited, then retried) — it can never be resumed,
            // so drop it now instead of leaving it to `main.rs`'s
            // `OAUTH_BROWSER_TIMEOUT` to reap five minutes later.
            if let Some(stale_state) = pending_sessions.take_stale_for(&name, &url) {
                pending_authorizations.cancel(&stale_state);
            }

            let secrets: Arc<dyn crate::secrets::SecretStore> = Arc::new(crate::secrets::OsKeyring);
            // Two instances built from the same backing store, per
            // `OAuthProvider`'s own doc comment: the manager needs one to
            // persist tokens into, `OAuthProvider` needs the other to clear
            // them on `invalidate`, and they must read/write the same
            // keychain entry or the two would silently disagree.
            let store_for_manager =
                crate::oauth_store::KeyringCredentialStore::new(secrets.clone(), &name);
            let store_for_provider: Arc<dyn uia_mcp::auth::CredentialStore> = Arc::new(
                crate::oauth_store::KeyringCredentialStore::new(secrets, &name),
            );

            let mut manager = uia_mcp::auth::AuthorizationManager::new(url.clone())
                .await
                .map_err(|e| RegistryError::Unreachable(e.to_string()))?;
            manager.set_credential_store(store_for_manager);
            let manager = Arc::new(manager);
            let provider: Arc<dyn uia_mcp::TokenProvider> = Arc::new(uia_mcp::OAuthProvider::new(
                manager.clone(),
                store_for_provider,
            ));

            let config = uia_mcp::McpServerConfig::Http {
                url: url.clone(),
                // Guaranteed empty by the guard above: an OAuth server never
                // carries a static header alongside its bearer token.
                headers: Vec::new(),
                expects_token: true,
            };
            let name_for_task = name.clone();
            let connect_and_list = async move {
                let executor = uia_mcp::McpExecutor::connect_with_token_provider(
                    &name_for_task,
                    config,
                    provider,
                )
                .await?;
                let declaration = executor.declaration();
                let tools = executor.list_tools().await?;
                Ok::<_, uia_core::tools::ToolError>((declaration, tools))
            };

            let outcome = tokio::time::timeout(PREVIEW_TIMEOUT, connect_and_list).await;

            let challenge = match outcome {
                Ok(Ok((declaration, tools))) => {
                    // Stored credentials already worked: phase 2, the retry
                    // after a completed sign-in. No header on an OAuth entry.
                    let mut entry = remote_entry_from(name, url, None, None, declaration, &tools)?;
                    entry.auth = RemoteAuth::OAuth;
                    entry.oauth_client_id = oauth_client_id;
                    return Ok(entry);
                }
                Ok(Err(uia_core::tools::ToolError::AuthRequired(challenge))) => challenge,
                Ok(Err(other)) => return Err(RegistryError::Unreachable(other.to_string())),
                Err(_elapsed) => {
                    return Err(RegistryError::Unreachable(
                        "the server did not answer in time".to_string(),
                    ));
                }
            };

            // Only one other strong reference to `manager` ever existed (the
            // clone `OAuthProvider` held), and that provider was dropped
            // along with the rest of `connect_and_list`'s captured state once
            // the future above resolved — so the manager is uniquely held
            // again here, and phase 1 (discovery + building the session) can
            // take it back by value.
            let mut manager = Arc::try_unwrap(manager).unwrap_or_else(|_| {
                unreachable!(
                    "no other strong reference to the OAuth manager should outlive its own \
                     failed connect attempt"
                )
            });

            // Seeds discovery from the real 401's own challenge instead of
            // guessing at well-known paths — the reactive path `with_challenge`
            // exists for. `AuthorizationSession::new` does not do this itself
            // (only the higher-level `OAuthState::start_authorization`, which
            // this flow does not use, does); it must happen here first.
            //
            // Finding 1 (2026-08-30 whole-branch review): both of these are
            // real HTTP round trips against the authorization server this
            // 401 named — resolving its metadata, then (absent a
            // pre-registered `oauth_client_id`) Dynamic Client Registration
            // against it — and neither was bounded before this fix. A slow
            // or black-holed authorization server would hang this whole
            // Tauri command, and therefore the Settings dialog, indefinitely.
            // Wrapped together, not separately, because both share the one
            // `OAUTH_DISCOVERY_TIMEOUT` budget and either failing the same
            // way (`Unreachable`) is all a caller can act on.
            let discovery_and_session = async move {
                let resolution = manager
                    .resolve_metadata_from_challenge(Some(&challenge))
                    .await
                    .map_err(|e| RegistryError::Unreachable(e.to_string()))?;
                manager.set_metadata(resolution.metadata);

                let mut request = uia_mcp::auth::AuthorizationRequest::new("uia://callback")
                    .with_challenge(challenge);
                if let Some(client_id) = oauth_client_id.clone() {
                    request = request.with_preregistered_client(client_id);
                }

                match uia_mcp::auth::AuthorizationSession::new(manager, request).await {
                    Ok(session) => Ok(session),
                    Err((_manager, e)) => Err(RegistryError::Unreachable(e.to_string())),
                }
            };

            let session = tokio::time::timeout(OAUTH_DISCOVERY_TIMEOUT, discovery_and_session)
                .await
                .map_err(|_| {
                    RegistryError::Unreachable(
                        "the authorization server did not answer in time".to_string(),
                    )
                })??;

            let authorize_url = session.get_authorization_url().to_string();
            let state = url::Url::parse(&authorize_url)
                .ok()
                .and_then(|parsed| {
                    parsed
                        .query_pairs()
                        .find(|(k, _)| k == "state")
                        .map(|(_, v)| v.into_owned())
                })
                .ok_or_else(|| {
                    RegistryError::Unreachable(
                        "the authorization server returned no state parameter".to_string(),
                    )
                })?;

            // Finding 3 (2026-08-30 whole-branch review): registers this
            // sign-in with `uia_mcp::callback::PendingAuthorizations` too, so
            // `oauth_flow::DeepLinkListener::deliver` can route the eventual
            // browser callback through rmcp's own validation instead of only
            // ever hand-parsing the raw redirect URL. See that method's doc
            // comment for how the two are combined.
            pending_authorizations.register(&state, &name);
            pending_sessions.insert(state.clone(), name, url, session);

            Err(RegistryError::NeedsAuthorization {
                authorize_url,
                state,
            })
        }
    }
}

/// Reconnects an already-added, already-signed-in OAuth remote using its
/// stored credentials — the ordinary per-session path, as opposed to
/// `preview_remote`'s one-time interactive add flow.
///
/// `preview_remote`'s `OAuthProvider` wiring only ever lived for the
/// duration of that one Tauri command; nothing reconnected it afterward, so
/// `session::build_executor` was connecting every enabled OAuth remote with
/// `McpExecutor::connect`'s bare `NoToken` — no bearer token ever sent, no
/// error surfaced beyond the server's own 401. This is that same wiring,
/// built fresh for a live session instead of a preview.
///
/// Proactively resolves authorization metadata and configures the OAuth
/// client from the entry's own `oauth_client_id`
/// (`AuthorizationManager::configure_client_id`) rather than waiting for a
/// live 401 to trigger it the way `preview_remote` does: this path has no
/// interactive fallback to fall into if a refresh is needed mid-session, so
/// the manager must already be able to refresh when the access token nears
/// its ~1h expiry, not just on the very first connect after sign-in.
pub async fn connect_oauth_remote(
    entry: &RemoteEntry,
) -> Result<uia_mcp::McpExecutor, uia_core::tools::ToolError> {
    use uia_core::tools::ToolError;

    let secrets: Arc<dyn crate::secrets::SecretStore> = Arc::new(crate::secrets::OsKeyring);
    // Same pairing `preview_remote` uses and the same reason: the manager
    // persists refreshed tokens through one instance, `OAuthProvider` reads/
    // clears them through the other, and both must point at the same
    // keychain entry.
    let store_for_manager =
        crate::oauth_store::KeyringCredentialStore::new(secrets.clone(), &entry.name);
    let store_for_provider: Arc<dyn uia_mcp::auth::CredentialStore> = Arc::new(
        crate::oauth_store::KeyringCredentialStore::new(secrets, &entry.name),
    );

    let mut manager = uia_mcp::auth::AuthorizationManager::new(entry.url.clone())
        .await
        .map_err(|e| ToolError::Transport(e.to_string()))?;
    manager.set_credential_store(store_for_manager);

    if let Some(client_id) = entry.oauth_client_id.as_ref() {
        let resolution = manager
            .resolve_metadata()
            .await
            .map_err(|e| ToolError::Transport(e.to_string()))?;
        manager.set_metadata(resolution.metadata);
        manager
            .configure_client_id(client_id)
            .map_err(|e| ToolError::Transport(e.to_string()))?;
    }

    let manager = Arc::new(manager);
    let provider: Arc<dyn uia_mcp::TokenProvider> =
        Arc::new(uia_mcp::OAuthProvider::new(manager, store_for_provider));

    let config = uia_mcp::McpServerConfig::Http {
        url: entry.url.clone(),
        // Guaranteed empty the same way `preview_remote`'s is: an OAuth
        // entry never carries a static `Authorization` header alongside it.
        headers: Vec::new(),
        expects_token: true,
    };
    uia_mcp::McpExecutor::connect_with_token_provider(&entry.name, config, provider).await
}

#[cfg(test)]
mod tests {
    use super::*;
    // The trait, for its `get`/`set` on `FakeSecretStore` below -- the module
    // itself only ever names it as `dyn crate::secrets::SecretStore`.

    fn temp_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uia-reg-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("uia-mcp.json")
    }

    fn servers_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uia-desc-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Lays down a bundle the way `install_bundle` would have. `server_type`
    /// is a parameter so a test can write one that no longer validates.
    fn installed_bundle(root: &Path, name: &str, server_type: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("server")).unwrap();
        std::fs::write(dir.join("server").join(name), b"\x7fELF\x02binary").unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{
                    "manifest_version": "0.2",
                    "name": "{name}",
                    "version": "1.0.0",
                    "server": {{
                        "type": "{server_type}",
                        "entry_point": "server/{name}",
                        "mcp_config": {{
                            "command": "${{__dirname}}/server/{name}",
                            "args": ["--stdio"],
                            "env": {{ "API_TOKEN": "sk-do-not-leak-me" }}
                        }}
                    }}
                }}"#
            ),
        )
        .unwrap();
    }

    fn registry_with(name: &str) -> McpRegistry {
        McpRegistry {
            local_servers: vec![LocalServerEntry {
                name: name.into(),
                version: Some("1.0.0".into()),
                enabled: true,
                user_config: BTreeMap::new(),
            }],
            remote_servers: vec![],
        }
    }

    /// The reason this function exists, and it could not be tested at all
    /// while it lived inside a `#[tauri::command]`: a removed OAuth server
    /// must not leave its tokens behind. Asserted against the store itself
    /// rather than through `load()`, which short-circuits on the main entry
    /// and would pass with a refresh token still sitting there.
    #[tokio::test]
    async fn removing_an_oauth_remote_destroys_its_saved_tokens() {
        let path = temp_path("remove-oauth");
        let secrets = Arc::new(crate::secrets::FakeSecretStore::new());
        secrets
            .set("mcp-oauth.docs.0", "{\"access\":\"token\"}")
            .unwrap();
        secrets.set("mcp-oauth.docs.count", "1").unwrap();
        secrets
            .set("mcp-oauth.docs.refresh.0", "refresh-token")
            .unwrap();
        secrets.set("mcp-oauth.docs.refresh.count", "1").unwrap();

        let mut entry = remote("docs");
        entry.auth = RemoteAuth::OAuth;
        entry.oauth_client_id = Some("client-id".into());
        save(
            &path,
            &McpRegistry {
                local_servers: vec![],
                remote_servers: vec![entry],
            },
        )
        .unwrap();

        remove_remote_and_clear_credentials(&path, "docs", secrets.clone())
            .await
            .unwrap();

        assert!(load(&path).remote_servers.is_empty(), "record must be gone");
        for account in [
            "mcp-oauth.docs.count",
            "mcp-oauth.docs.0",
            "mcp-oauth.docs.refresh.count",
            "mcp-oauth.docs.refresh.0",
        ] {
            assert!(
                secrets.get(account).is_none(),
                "{account} survived removal — a token the user believes they \
                 deleted is still in the keystore"
            );
        }
    }

    /// A static-header server has no keychain entry, and `SecretStore::delete`
    /// errors on an account that does not exist. Removing one must therefore
    /// not attempt the clear at all — otherwise every such removal reports a
    /// failure for a credential that was never supposed to be there.
    #[tokio::test]
    async fn removing_a_static_header_remote_does_not_attempt_a_credential_clear() {
        let path = temp_path("remove-static");
        let secrets = Arc::new(crate::secrets::FakeSecretStore::new());
        save(
            &path,
            &McpRegistry {
                local_servers: vec![],
                remote_servers: vec![remote("docs")],
            },
        )
        .unwrap();

        remove_remote_and_clear_credentials(&path, "docs", secrets)
            .await
            .expect("removing a static-header server must not fail on credentials");

        assert!(load(&path).remote_servers.is_empty());
    }

    #[test]
    fn describing_a_local_server_resolves_what_it_would_launch() {
        let root = servers_root("describe-ok");
        installed_bundle(&root, "clock", "binary");

        let details = describe_local_server(&registry_with("clock"), &root, "clock").unwrap();
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(details.version.as_deref(), Some("1.0.0"));
        assert_eq!(details.args, vec!["--stdio".to_string()]);
        assert!(
            details.command.ends_with("clock") && details.command.contains("server"),
            "the command must be the resolved path inside the bundle, got {:?}",
            details.command
        );
    }

    /// The environment can carry a token, so only the NAMES cross the IPC
    /// boundary — the same rule that keeps a remote's header value off it.
    /// Asserted on the whole serialized struct, because a field added later
    /// that happened to carry the value would otherwise slip through.
    #[test]
    fn describing_a_local_server_reports_env_names_but_never_their_values() {
        let root = servers_root("describe-env");
        installed_bundle(&root, "clock", "binary");

        let details = describe_local_server(&registry_with("clock"), &root, "clock").unwrap();
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(details.env_keys, vec!["API_TOKEN".to_string()]);
        let serialized = serde_json::to_string(&details).unwrap();
        assert!(
            !serialized.contains("sk-do-not-leak-me"),
            "an env VALUE reached the frontend: {serialized}"
        );
    }

    #[test]
    fn describing_a_server_that_is_not_installed_says_so() {
        let root = servers_root("describe-absent");
        let err = describe_local_server(&McpRegistry::default(), &root, "ghost").unwrap_err();
        std::fs::remove_dir_all(&root).ok();
        assert!(err.contains("no local server named"), "got {err:?}");
    }

    /// Same wording `mcp_targets` uses when it skips this server at launch,
    /// so the Details panel and the Status column describe one problem in one
    /// voice rather than two.
    #[test]
    fn describing_a_server_whose_files_are_gone_reports_the_same_reason_as_launch() {
        let root = servers_root("describe-missing");
        let err = describe_local_server(&registry_with("ghost"), &root, "ghost").unwrap_err();
        std::fs::remove_dir_all(&root).ok();
        assert!(err.contains("installed files are missing"), "got {err:?}");
    }

    /// A bundle rewritten after install to run an interpreter must be
    /// reported as failing validation, not described as if it were fine --
    /// this is the panel's view of the binary-only rule.
    #[test]
    fn describing_a_server_whose_manifest_was_rewritten_reports_the_validation_failure() {
        let root = servers_root("describe-tampered");
        installed_bundle(&root, "clock", "node");

        let err = describe_local_server(&registry_with("clock"), &root, "clock").unwrap_err();
        std::fs::remove_dir_all(&root).ok();

        assert!(
            err.contains("no longer passes bundle validation"),
            "got {err:?}"
        );
    }

    fn remote(name: &str) -> RemoteEntry {
        RemoteEntry {
            name: name.into(),
            url: format!("https://{name}.test/mcp"),
            header_name: None,
            header_value: None,
            enabled: false,
            declared_name: Some(format!("{name}-server")),
            declared_version: Some("2.0.0".into()),
            tools: vec![DeclaredTool {
                name: "search".into(),
                description: "Search the index".into(),
            }],
            auth: RemoteAuth::Static,
            oauth_client_id: None,
        }
    }

    /// The sidecar convention every other settings file follows.
    #[test]
    fn the_registry_and_local_servers_dir_are_siblings_of_the_config_file() {
        let cfg = Path::new("/somewhere/uia-client/uia.toml");
        assert_eq!(
            mcp_registry_path_for(cfg),
            PathBuf::from("/somewhere/uia-client/uia-mcp.json")
        );
        assert_eq!(
            mcp_local_servers_dir_for(cfg),
            PathBuf::from("/somewhere/uia-client/mcp-local-servers")
        );
    }

    /// A fresh install talks to nothing until the user adds something. Each
    /// server is trusted individually, by its own `enabled` flag; there is no
    /// registry-wide switch that could arrive pre-set from a restored file.
    #[test]
    fn a_fresh_registry_holds_nothing() {
        let reg = McpRegistry::default();
        assert!(reg.local_servers.is_empty());
        assert!(reg.remote_servers.is_empty());
    }

    /// A registry written before `allow_remote` was removed and before
    /// `plugins` was renamed must still load the servers it holds.
    ///
    /// This is the one place the rename could have destroyed real data
    /// silently rather than loudly: `load` maps a parse failure to
    /// `Default::default()`, so a registry this refuses to read comes back
    /// as an empty one — every remote server the user added simply gone,
    /// with no error anywhere. The guarantee being pinned is that serde
    /// IGNORES the two dead keys (there is no `deny_unknown_fields` on
    /// `McpRegistry`, and adding one would turn every old file into an
    /// empty registry).
    #[test]
    fn a_registry_holding_the_pre_rename_keys_still_loads_its_remotes() {
        let path = temp_path("pre-rename");
        std::fs::write(
            &path,
            r#"{
                "allow_remote": true,
                "plugins": [],
                "remote_servers": [
                    {
                        "name": "docs",
                        "url": "https://example.com/mcp",
                        "header_name": null,
                        "header_value": null,
                        "enabled": true,
                        "declared_name": "docs",
                        "declared_version": "1.0.0",
                        "tools": [],
                        "auth": "oauth",
                        "oauth_client_id": "abc123"
                    }
                ]
            }"#,
        )
        .unwrap();

        let reg = load(&path);
        std::fs::remove_file(&path).ok();

        assert_eq!(
            reg.remote_servers.len(),
            1,
            "the dead `allow_remote`/`plugins` keys must be ignored, not fatal"
        );
        assert_eq!(reg.remote_servers[0].name, "docs");
        assert!(reg.remote_servers[0].enabled);
        assert!(reg.local_servers.is_empty());
    }

    /// Same tolerance as `settings::load`: a missing or corrupt sidecar must
    /// never block startup.
    #[test]
    fn a_missing_or_corrupt_file_loads_as_the_default_registry() {
        let path = temp_path("missing");
        assert_eq!(load(&path), McpRegistry::default());

        std::fs::write(&path, "{ not json").unwrap();
        let loaded = load(&path);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        assert_eq!(loaded, McpRegistry::default());
    }

    #[test]
    fn a_saved_registry_round_trips_every_field() {
        let path = temp_path("roundtrip");
        let mut reg = McpRegistry {
            local_servers: vec![LocalServerEntry {
                name: "clock".into(),
                version: Some("1.4.0".into()),
                enabled: true,
                user_config: BTreeMap::new(),
            }],
            remote_servers: vec![remote("docs")],
        };
        reg.remote_servers[0].header_name = Some("Authorization".into());
        reg.remote_servers[0].header_value = Some("Bearer t".into());

        save(&path, &reg).unwrap();
        let back = load(&path);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        assert_eq!(back, reg);
    }

    #[test]
    fn enabling_and_removing_a_local_server_changes_only_that_one() {
        let mut reg = McpRegistry {
            local_servers: vec![
                LocalServerEntry {
                    name: "clock".into(),
                    version: None,
                    enabled: false,
                    user_config: BTreeMap::new(),
                },
                LocalServerEntry {
                    name: "files".into(),
                    version: None,
                    enabled: false,
                    user_config: BTreeMap::new(),
                },
            ],
            remote_servers: vec![],
        };

        reg.set_local_server_enabled("files", true).unwrap();
        assert!(!reg.local_servers[0].enabled, "clock must be untouched");
        assert!(reg.local_servers[1].enabled);

        reg.remove_local_server("clock").unwrap();
        let names: Vec<&str> = reg.local_servers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["files"]);
    }

    #[test]
    fn acting_on_a_local_server_that_is_not_installed_is_an_error() {
        let mut reg = McpRegistry::default();
        assert!(matches!(
            reg.set_local_server_enabled("ghost", true),
            Err(RegistryError::NoSuchLocalServer(_))
        ));
        assert!(matches!(
            reg.remove_local_server("ghost"),
            Err(RegistryError::NoSuchLocalServer(_))
        ));
    }

    /// Installing is a deliberate first step; enabling is a deliberate second
    /// one. Folding the two together — a local server arriving already trusted just
    /// because it was added — would make the second step invisible, the same
    /// reasoning as `an_added_remote_server_starts_disabled` below.
    #[test]
    fn an_added_local_server_starts_disabled() {
        let mut reg = McpRegistry::default();

        // add_local_server's signature has no `enabled` field to set, but the
        // property under test is that there is no way to add a local server that
        // is already enabled — the caller cannot ask for it either.
        reg.add_local_server("clock".into(), Some("1.0.0".into()))
            .unwrap();

        assert_eq!(reg.local_servers.len(), 1);
        assert!(
            !reg.local_servers[0].enabled,
            "a new local server must start disabled"
        );
        assert_eq!(reg.local_servers[0].name, "clock");
        assert_eq!(reg.local_servers[0].version.as_deref(), Some("1.0.0"));
    }

    /// The name prefixes every tool name, and `McpRouter` dispatches a
    /// `server.tool` call on the `server` half alone. A local server sharing a
    /// name with an existing local server OR an existing remote server would make
    /// that routing ambiguous, so both directions must be refused.
    #[test]
    fn a_duplicate_name_is_refused_against_local_servers_and_remotes() {
        let mut reg = McpRegistry::default();
        reg.add_local_server("clock".into(), None).unwrap();
        assert!(matches!(
            reg.add_local_server("clock".into(), None),
            Err(RegistryError::DuplicateName(_))
        ));

        reg.add_remote(remote("wiki")).unwrap();
        assert!(matches!(
            reg.add_local_server("wiki".into(), None),
            Err(RegistryError::DuplicateName(_))
        ));
    }

    /// Adding is a deliberate first step; enabling is a deliberate second
    /// one. A server that arrived already trusted would collapse the two.
    #[test]
    fn an_added_remote_server_starts_disabled() {
        let mut reg = McpRegistry::default();
        let mut entry = remote("docs");
        entry.enabled = true; // even if the caller asks for it

        reg.add_remote(entry).unwrap();

        assert_eq!(reg.remote_servers.len(), 1);
        assert!(
            !reg.remote_servers[0].enabled,
            "a new remote must start disabled"
        );
        assert_eq!(
            reg.remote_servers[0].declared_name.as_deref(),
            Some("docs-server")
        );
        assert_eq!(reg.remote_servers[0].tools[0].name, "search");
    }

    /// `RemoteEntry`'s `Debug` is hand-written, ON PURPOSE, to redact
    /// `header_value` — do not "clean this up" back to a derive. This test
    /// proves the redaction at both the level that would be easy to notice
    /// (formatting the entry itself) and the level that is easy to miss
    /// (formatting anything that merely contains one, `McpRegistry`
    /// included). The second assertion is the one that matters: it proves
    /// derived `Debug` on `McpRegistry` actually delegates to `RemoteEntry`'s
    /// own impl rather than needing its own redaction.
    #[test]
    fn debug_output_redacts_a_remote_header_value_at_every_level_that_contains_one() {
        let mut entry = remote("docs");
        entry.header_name = Some("Authorization".into());
        entry.header_value = Some("Bearer super-secret-token".into());

        let entry_printed = format!("{entry:?}");
        assert!(
            !entry_printed.contains("super-secret-token"),
            "the auth token leaked into RemoteEntry Debug output: {entry_printed}"
        );
        assert!(entry_printed.contains("docs"), "{entry_printed}");
        assert!(entry_printed.contains("Authorization"), "{entry_printed}");

        let reg = McpRegistry {
            local_servers: vec![],
            remote_servers: vec![entry],
        };
        let registry_printed = format!("{reg:?}");
        assert!(
            !registry_printed.contains("super-secret-token"),
            "the auth token leaked into McpRegistry Debug output: {registry_printed}"
        );
        assert!(registry_printed.contains("docs"), "{registry_printed}");
        assert!(
            registry_printed.contains("Authorization"),
            "{registry_printed}"
        );
    }

    /// The name prefixes every tool name, and OpenAI rejects the whole
    /// declaration over one bad character.
    #[test]
    fn a_remote_name_that_would_break_the_tool_declaration_is_refused() {
        let mut reg = McpRegistry::default();
        let mut entry = remote("docs");
        entry.name = "docs.remote".into();
        assert!(matches!(reg.add_remote(entry), Err(RegistryError::Name(_))));
    }

    /// Two servers sharing a namespace makes tool routing ambiguous, and
    /// `McpRouter` dispatches on that namespace alone.
    #[test]
    fn a_duplicate_remote_name_is_refused() {
        let mut reg = McpRegistry::default();
        reg.add_remote(remote("docs")).unwrap();
        assert!(matches!(
            reg.add_remote(remote("docs")),
            Err(RegistryError::DuplicateName(_))
        ));
    }

    /// A remote must not be able to shadow an installed local server
    /// either — same namespace, same ambiguity.
    #[test]
    fn a_remote_may_not_take_an_installed_local_servers_name() {
        let mut reg = McpRegistry {
            local_servers: vec![LocalServerEntry {
                name: "docs".into(),
                version: None,
                enabled: false,
                user_config: BTreeMap::new(),
            }],
            remote_servers: vec![],
        };
        assert!(matches!(
            reg.add_remote(remote("docs")),
            Err(RegistryError::DuplicateName(_))
        ));
    }

    #[test]
    fn enabling_and_removing_a_remote_changes_only_that_server() {
        let mut reg = McpRegistry::default();
        reg.add_remote(remote("docs")).unwrap();
        reg.add_remote(remote("wiki")).unwrap();

        reg.set_remote_enabled("wiki", true).unwrap();
        assert!(!reg.remote_servers[0].enabled);
        assert!(reg.remote_servers[1].enabled);

        reg.remove_remote("docs").unwrap();
        let names: Vec<&str> = reg.remote_servers.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["wiki"]);
    }

    #[test]
    fn acting_on_a_remote_that_was_never_added_is_an_error() {
        let mut reg = McpRegistry::default();
        assert!(matches!(
            reg.set_remote_enabled("ghost", true),
            Err(RegistryError::NoSuchRemote(_))
        ));
        assert!(matches!(
            reg.remove_remote("ghost"),
            Err(RegistryError::NoSuchRemote(_))
        ));
    }

    use uia_core::tools::ToolDescriptor;
    use uia_mcp::ServerDeclaration;

    fn descriptor(name: &str, description: &str) -> ToolDescriptor {
        ToolDescriptor {
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::json!({}),
            requires_confirmation: false,
        }
    }

    /// The mapping a live connection feeds: what the server said becomes
    /// what Settings will show.
    #[test]
    fn a_declaration_and_tool_list_become_a_storable_entry() {
        let entry = remote_entry_from(
            "docs".into(),
            "https://docs.test/mcp".into(),
            Some("Authorization".into()),
            Some("Bearer t".into()),
            Some(ServerDeclaration {
                name: "docs-mcp".into(),
                version: Some("2.1.0".into()),
            }),
            &[descriptor("docs.search", "Search the docs")],
        )
        .unwrap();

        assert_eq!(entry.name, "docs");
        assert_eq!(entry.declared_name.as_deref(), Some("docs-mcp"));
        assert_eq!(entry.declared_version.as_deref(), Some("2.1.0"));
        assert_eq!(entry.tools.len(), 1);
        assert_eq!(entry.tools[0].name, "docs.search");
        assert_eq!(entry.tools[0].description, "Search the docs");
        assert!(!entry.enabled, "a previewed server is not yet trusted");
    }

    /// A server that answers but offers nothing has declared no capability,
    /// so there is nothing for the user to approve.
    #[test]
    fn a_server_declaring_no_tools_is_refused() {
        let err = remote_entry_from(
            "empty".into(),
            "https://empty.test/mcp".into(),
            None,
            None,
            Some(ServerDeclaration {
                name: "empty".into(),
                version: None,
            }),
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, RegistryError::NoDeclaredTools), "got {err:?}");
    }

    /// The name is validated before anything is stored, for the same reason
    /// it is validated on add: it prefixes every tool name.
    #[test]
    fn a_previewed_name_that_would_break_the_tool_declaration_is_refused() {
        let err = remote_entry_from(
            "docs.remote".into(),
            "https://docs.test/mcp".into(),
            None,
            None,
            Some(ServerDeclaration {
                name: "d".into(),
                version: None,
            }),
            &[descriptor("t", "d")],
        )
        .unwrap_err();
        assert!(matches!(err, RegistryError::Name(_)), "got {err:?}");
    }

    /// A bare hostname or a pasted value with a stray space is the typo
    /// this field is actually at risk of.
    #[test]
    fn a_url_that_is_not_http_is_refused_before_any_connection() {
        for bad in [
            "docs.test/mcp",
            "ftp://docs.test/mcp",
            "https://",
            "https://a b/mcp",
        ] {
            let err = remote_entry_from(
                "docs".into(),
                bad.into(),
                None,
                None,
                Some(ServerDeclaration {
                    name: "d".into(),
                    version: None,
                }),
                &[descriptor("t", "d")],
            )
            .unwrap_err();
            assert!(
                matches!(err, RegistryError::InvalidUrl(_)),
                "{bad} gave {err:?}"
            );
        }
    }

    /// A URL nothing answers on must fail at add time, where the user can
    /// still fix it, rather than becoming a saved entry that silently never
    /// works.
    #[tokio::test]
    async fn a_url_that_never_answers_initialize_is_refused() {
        // Port 1 is reserved and never listening, so this fails to connect
        // rather than hanging on a real service.
        let pending = PendingOAuthSessions::default();
        let pending_auth = uia_mcp::callback::PendingAuthorizations::default();
        let err = preview_remote(
            "dead".into(),
            "http://127.0.0.1:1/mcp".into(),
            None,
            None,
            None,
            RemoteAuth::Static,
            &pending,
            &pending_auth,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RegistryError::Unreachable(_)), "got {err:?}");
    }

    // --- Task 7: the OAuth preview flow -----------------------------------
    //
    // A local axum server standing in for both the MCP endpoint and its own
    // authorization server, reusing `uia-mcp/src/client.rs`'s
    // local-axum-server test pattern rather than inventing a new one.
    //
    // The brief's own sketch of the fully-specified test passes a fixed,
    // illustrative challenge string (`Bearer resource_metadata="https://
    // as.test/..."`). That string cannot be used verbatim here: rmcp's real
    // `AuthorizationManager::resolve_metadata_from_challenge` performs a
    // genuine HTTP GET against the `resource_metadata` URL a 401's challenge
    // names, and `AuthorizationSession::new`'s `configure_client` hard-requires
    // that discovery to have already resolved authorization-server metadata
    // before it can build an authorize URL at all (confirmed against rmcp
    // 3.1.4's `auth.rs`: a discovery network failure is a hard `Err`, not a
    // graceful fallback, and `configure_client` returns
    // `AuthError::NoAuthorizationSupport` with no metadata). `as.test` is
    // IANA's reserved, guaranteed-unresolvable test TLD, so a real discovery
    // attempt against it always fails before a `NeedsAuthorization` could
    // ever be produced — see task-7-report.md for the full trace. This
    // helper therefore points the challenge at this same local server's own
    // address, and serves the protected-resource and authorization-server
    // metadata documents discovery needs to succeed for real.
    struct OAuthTestServer {
        addr: std::net::SocketAddr,
        unauthorized_hits: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl OAuthTestServer {
        fn url(&self) -> String {
            format!("http://{}/mcp", self.addr)
        }
    }

    /// Spins up a server that always answers the MCP endpoint with 401 plus a
    /// challenge pointing at its own metadata documents — never grants a
    /// token, so every connect attempt (`preview_remote`'s first phase, and
    /// any accidental retry) observes the same 401.
    async fn server_answering_401_with_challenge() -> OAuthTestServer {
        use axum::Router;
        use axum::http::{HeaderMap, StatusCode, header::WWW_AUTHENTICATE};
        use axum::response::IntoResponse;
        use axum::routing::{get, post};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let unauthorized_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let challenge = format!(
            "Bearer resource_metadata=\"http://{addr}/.well-known/oauth-protected-resource\""
        );
        let hits = unauthorized_hits.clone();
        let mcp_challenge = challenge.clone();
        let app = Router::new()
            .route(
                "/mcp",
                post(move || {
                    let hits = hits.clone();
                    let challenge = mcp_challenge.clone();
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let mut headers = HeaderMap::new();
                        headers.insert(WWW_AUTHENTICATE, challenge.parse().unwrap());
                        (StatusCode::UNAUTHORIZED, headers, "unauthorized")
                    }
                }),
            )
            .route(
                "/.well-known/oauth-protected-resource",
                get(move || async move {
                    axum::Json(serde_json::json!({
                        "resource": format!("http://{addr}/mcp"),
                        "authorization_servers": [format!("http://{addr}")],
                    }))
                    .into_response()
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(move || async move {
                    axum::Json(serde_json::json!({
                        "issuer": format!("http://{addr}"),
                        "authorization_endpoint": format!("http://{addr}/authorize"),
                        "token_endpoint": format!("http://{addr}/token"),
                        "registration_endpoint": format!("http://{addr}/register"),
                        "response_types_supported": ["code"],
                    }))
                    .into_response()
                }),
            )
            // Dynamic Client Registration: the fallback path when a caller
            // gives `preview_remote` no pre-registered `oauth_client_id` (the
            // brief's own fully-specified test does not pass one). Without
            // this route `AuthorizationSession::new` cannot obtain a
            // `client_id` at all and fails before ever reaching the
            // authorize URL this task's whole feature hinges on.
            .route(
                "/register",
                post(|| async move {
                    axum::Json(serde_json::json!({
                        "client_id": "dynamically-registered-client",
                        "redirect_uris": ["uia://callback"],
                    }))
                    .into_response()
                }),
            );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        OAuthTestServer {
            addr,
            unauthorized_hits,
        }
    }

    /// Same 401-plus-challenge shape as `server_answering_401_with_challenge`,
    /// except its authorization-server metadata document never answers at
    /// all — proving finding 1's fix: `resolve_metadata_from_challenge`'s own
    /// HTTP GET against this endpoint is what the OAUTH_DISCOVERY_TIMEOUT
    /// wrap in `preview_remote`'s OAuth branch must bound. Before that fix
    /// this call had no timeout of its own, so a server this unresponsive
    /// would hang `preview_remote` (and therefore the Tauri command, and the
    /// Settings dialog) forever rather than failing.
    async fn server_with_hanging_authorization_server_metadata() -> OAuthTestServer {
        use axum::Router;
        use axum::http::{HeaderMap, StatusCode, header::WWW_AUTHENTICATE};
        use axum::response::IntoResponse;
        use axum::routing::{get, post};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let unauthorized_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let challenge = format!(
            "Bearer resource_metadata=\"http://{addr}/.well-known/oauth-protected-resource\""
        );
        let hits = unauthorized_hits.clone();
        let mcp_challenge = challenge.clone();
        let app = Router::new()
            .route(
                "/mcp",
                post(move || {
                    let hits = hits.clone();
                    let challenge = mcp_challenge.clone();
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let mut headers = HeaderMap::new();
                        headers.insert(WWW_AUTHENTICATE, challenge.parse().unwrap());
                        (StatusCode::UNAUTHORIZED, headers, "unauthorized")
                    }
                }),
            )
            .route(
                "/.well-known/oauth-protected-resource",
                get(move || async move {
                    axum::Json(serde_json::json!({
                        "resource": format!("http://{addr}/mcp"),
                        "authorization_servers": [format!("http://{addr}")],
                    }))
                    .into_response()
                }),
            )
            // Deliberately never resolves — the endpoint discovery follows
            // straight after the (fast) protected-resource document above.
            .route(
                "/.well-known/oauth-authorization-server",
                get(|| async move {
                    std::future::pending::<()>().await;
                    #[allow(unreachable_code)]
                    StatusCode::OK
                }),
            );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        OAuthTestServer {
            addr,
            unauthorized_hits,
        }
    }

    /// The bug this task fixes: today every failure collapses into
    /// `Unreachable`, so an authenticating server looks broken and can never
    /// be added. The distinction is the whole feature.
    #[tokio::test]
    async fn a_401_is_reported_as_needing_authorization_not_as_unreachable() {
        let server = server_answering_401_with_challenge().await;
        let pending = PendingOAuthSessions::default();
        let pending_auth = uia_mcp::callback::PendingAuthorizations::default();
        let err = preview_remote(
            "example-server".into(),
            server.url(),
            None,
            None,
            None,
            RemoteAuth::OAuth,
            &pending,
            &pending_auth,
        )
        .await
        .expect_err("a 401 is not a successful preview");
        assert!(
            matches!(err, RegistryError::NeedsAuthorization { .. }),
            "got {err:?}"
        );
        // `AuthedHttpClient` retries a 401 exactly once (Task 1's plumbing)
        // before it surfaces; with no credentials ever available, both
        // attempts hit the server.
        assert_eq!(
            server
                .unauthorized_hits
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "expected the initial attempt plus exactly one retry"
        );
    }

    /// rmcp's `with_challenge` seeds discovery from the real 401 rather than
    /// guessing at well-known paths. Losing the header here would silently
    /// fall back to probing.
    ///
    /// Asserted the way that is actually observable from this layer: the
    /// resulting `authorize_url` must point at the authorization endpoint
    /// this test server's OWN metadata document names
    /// (`http://<addr>/authorize`) — not some other guessed path — which is
    /// only possible if the challenge's `resource_metadata` pointer was
    /// followed. A test asserting "probing never happened" is not available
    /// here without instrumenting rmcp's internals; asserting the
    /// downstream *effect* of successful challenge-seeded discovery is the
    /// cleanest substitute given the real API surface.
    #[tokio::test]
    async fn the_challenge_is_carried_so_discovery_need_not_probe() {
        let server = server_answering_401_with_challenge().await;
        let pending = PendingOAuthSessions::default();
        let pending_auth = uia_mcp::callback::PendingAuthorizations::default();
        let err = preview_remote(
            "example-server".into(),
            server.url(),
            None,
            None,
            Some("test-client-id".into()),
            RemoteAuth::OAuth,
            &pending,
            &pending_auth,
        )
        .await
        .expect_err("a 401 is not a successful preview");

        let RegistryError::NeedsAuthorization { authorize_url, .. } = err else {
            panic!("got {err:?}");
        };
        let expected_authorize_endpoint = format!("http://{}/authorize", server.addr);
        assert!(
            authorize_url.starts_with(&expected_authorize_endpoint),
            "authorize_url {authorize_url:?} was not built from this server's own \
             challenge-discovered metadata"
        );
    }

    /// The regression guard. This path must be untouched: a `Static` entry's
    /// transport is built exactly as it was before this task (no token mode,
    /// the pasted header still travels), and a `Static` preview that cannot
    /// connect still fails as a plain `Unreachable`, never as
    /// `NeedsAuthorization` — the OAuth branch above must be genuinely
    /// gated on `auth`, not entered unconditionally.
    ///
    /// A full successful round trip (a real streamable-HTTP MCP server
    /// answering `initialize`/`tools/list`) is not built here: no existing
    /// test in this codebase stands one up (`uia-mcp/src/client.rs`'s own
    /// HTTP tests are all against a dead port or a deliberate redirect
    /// failure — the only *successful* connect test uses a real `npx`
    /// stdio server and is `#[ignore]`d as a manual test), and hand-rolling
    /// a compliant streamable-HTTP JSON-RPC/SSE server from scratch is out
    /// of scope for a regression guard whose job is "this specific path was
    /// not accidentally changed". The existing `a_declaration_and_tool_list_
    /// become_a_storable_entry` test already covers the mapping a
    /// successful connection feeds into; what is new and worth guarding
    /// here is that `auth` genuinely selects the branch.
    #[test]
    fn a_static_header_server_still_previews_exactly_as_before() {
        let entry = RemoteEntry {
            name: "docs".into(),
            url: "https://docs.test/mcp".into(),
            header_name: Some("Authorization".into()),
            header_value: Some("Bearer pat".into()),
            auth: RemoteAuth::Static,
            ..Default::default()
        };
        let McpServerConfig::Http {
            headers,
            expects_token,
            ..
        } = entry.transport()
        else {
            panic!("HTTP entries must build an Http transport");
        };
        assert!(!expects_token, "a Static entry must never expect a token");
        assert_eq!(
            headers,
            vec![("Authorization".to_string(), "Bearer pat".to_string())],
            "the pasted header must still travel exactly as before this task"
        );
    }

    /// The same guard, exercised through `preview_remote` itself rather than
    /// `transport()` directly: a `Static` server that cannot be reached
    /// fails as `Unreachable`, the same failure mode as before this task —
    /// never `NeedsAuthorization`, which only the `OAuth` branch can return.
    #[tokio::test]
    async fn a_static_preview_that_cannot_connect_fails_as_unreachable_not_as_needing_auth() {
        let pending = PendingOAuthSessions::default();
        let pending_auth = uia_mcp::callback::PendingAuthorizations::default();
        let err = preview_remote(
            "docs".into(),
            "http://127.0.0.1:1/mcp".into(),
            Some("Authorization".into()),
            Some("Bearer pat".into()),
            None,
            RemoteAuth::Static,
            &pending,
            &pending_auth,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RegistryError::Unreachable(_)), "got {err:?}");
    }

    /// Task 2's rule, enforced where the user actually enters it: an
    /// `OAuth` entry that also carries a pasted `Authorization` header is
    /// refused before any connection is attempted, with a clear message
    /// rather than the generic collision error `http_transport_config`
    /// would eventually raise on a real connect.
    #[test]
    fn an_oauth_entry_refuses_a_pasted_authorization_header() {
        let mut entry = remote("docs");
        entry.auth = RemoteAuth::OAuth;
        entry.header_name = Some("Authorization".into());
        entry.header_value = Some("Bearer pasted".into());

        // `preview_remote` is async and needs a connection this test does not
        // want to make; the guard runs before any `.await` point, so driving
        // it to completion on a throwaway runtime is enough to prove it never
        // reaches the network.
        let pending = PendingOAuthSessions::default();
        let pending_auth = uia_mcp::callback::PendingAuthorizations::default();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(preview_remote(
                entry.name,
                entry.url,
                entry.header_name,
                entry.header_value,
                None,
                RemoteAuth::OAuth,
                &pending,
                &pending_auth,
            ));

        assert!(
            matches!(result, Err(RegistryError::OAuthHeaderConflict)),
            "got {result:?}"
        );
    }

    // --- Finding 1 (2026-08-30 whole-branch review): the discovery leg is
    // bounded -----------------------------------------------------------

    /// Before this fix, `resolve_metadata_from_challenge`'s HTTP GET (and
    /// the DCR call after it) ran with no timeout at all — only the initial
    /// connect attempt was wrapped in `PREVIEW_TIMEOUT`. A server that
    /// answers the 401 promptly but never answers its own metadata document
    /// would hang `preview_remote` forever. `OAUTH_DISCOVERY_TIMEOUT` is
    /// shrunk to 200ms under `#[cfg(test)]` (see its doc comment) so this
    /// proves the bound fires without costing real wall-clock time.
    #[tokio::test]
    async fn a_hanging_discovery_endpoint_is_bounded_by_its_own_timeout() {
        let server = server_with_hanging_authorization_server_metadata().await;
        let pending = PendingOAuthSessions::default();
        let pending_auth = uia_mcp::callback::PendingAuthorizations::default();

        let started = std::time::Instant::now();
        let err = preview_remote(
            "example-server".into(),
            server.url(),
            None,
            None,
            None,
            RemoteAuth::OAuth,
            &pending,
            &pending_auth,
        )
        .await
        .expect_err("a hanging authorization server must not hang preview_remote forever");

        assert!(matches!(err, RegistryError::Unreachable(_)), "got {err:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "preview_remote took {:?}, far longer than the test's 200ms \
             OAUTH_DISCOVERY_TIMEOUT should ever allow",
            started.elapsed()
        );
    }

    // --- Finding 2 (2026-08-30 whole-branch review): a retry resumes the
    // pending session instead of duplicating it -------------------------

    /// The bug: every retry (the frontend's "check again" button, clicked
    /// before the human finishes signing in) used to build a brand new
    /// `AuthorizationManager`/`AuthorizationSession` with a fresh CSRF
    /// `state`, silently abandoning the one already in flight. This proves
    /// the fix — two `preview_remote` calls for the same `(name, url)`
    /// before any callback arrives must return the SAME `state` and
    /// `authorize_url`, and only one session may be left pending.
    #[tokio::test]
    async fn a_repeated_preview_before_signing_in_resumes_the_same_pending_session() {
        let server = server_answering_401_with_challenge().await;
        let pending = PendingOAuthSessions::default();
        let pending_auth = uia_mcp::callback::PendingAuthorizations::default();

        let first = preview_remote(
            "example-server".into(),
            server.url(),
            None,
            None,
            Some("test-client-id".into()),
            RemoteAuth::OAuth,
            &pending,
            &pending_auth,
        )
        .await
        .expect_err("a 401 is not a successful preview");
        let RegistryError::NeedsAuthorization {
            authorize_url: url1,
            state: state1,
        } = first
        else {
            panic!("expected NeedsAuthorization, got something else");
        };

        let second = preview_remote(
            "example-server".into(),
            server.url(),
            None,
            None,
            Some("test-client-id".into()),
            RemoteAuth::OAuth,
            &pending,
            &pending_auth,
        )
        .await
        .expect_err("a 401 is not a successful preview");
        let RegistryError::NeedsAuthorization {
            authorize_url: url2,
            state: state2,
        } = second
        else {
            panic!("expected NeedsAuthorization, got something else");
        };

        assert_eq!(
            state1, state2,
            "a repeated preview before signing in must resume the same \
             pending session, not mint a fresh CSRF state"
        );
        assert_eq!(
            url1, url2,
            "the authorize_url must be resumed identically too"
        );
        assert_eq!(
            pending.pending_count(),
            1,
            "the second call must not have left a second, abandoned session pending"
        );
    }

    /// A retry for the same NAME but a different URL (the form was edited)
    /// is a genuinely different request and must NOT resume the old
    /// session's `authorize_url` — that would point sign-in at the wrong
    /// server. The stale entry is dropped rather than left pending.
    #[tokio::test]
    async fn a_retry_with_a_different_url_supersedes_rather_than_resumes() {
        let first_server = server_answering_401_with_challenge().await;
        let second_server = server_answering_401_with_challenge().await;
        let pending = PendingOAuthSessions::default();
        let pending_auth = uia_mcp::callback::PendingAuthorizations::default();

        let first = preview_remote(
            "example-server".into(),
            first_server.url(),
            None,
            None,
            Some("test-client-id".into()),
            RemoteAuth::OAuth,
            &pending,
            &pending_auth,
        )
        .await
        .expect_err("a 401 is not a successful preview");
        let RegistryError::NeedsAuthorization { state: state1, .. } = first else {
            panic!("expected NeedsAuthorization, got something else");
        };

        let second = preview_remote(
            "example-server".into(),
            second_server.url(),
            None,
            None,
            Some("test-client-id".into()),
            RemoteAuth::OAuth,
            &pending,
            &pending_auth,
        )
        .await
        .expect_err("a 401 is not a successful preview");
        let RegistryError::NeedsAuthorization {
            authorize_url: url2,
            state: state2,
        } = second
        else {
            panic!("expected NeedsAuthorization, got something else");
        };

        assert_ne!(
            state1, state2,
            "a different url must mint a genuinely new session"
        );
        assert!(
            url2.starts_with(&format!("http://{}/authorize", second_server.addr)),
            "the resumed authorize_url must point at the retried server, not the first one: \
             {url2:?}"
        );
        assert_eq!(
            pending.pending_count(),
            1,
            "the stale session for the old url must have been dropped, not left pending \
             alongside the new one"
        );
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;
    use crate::secrets::FakeSecretStore;

    const MANIFEST: &str = r#"{
        "manifest_version": "0.3", "name": "mymy", "version": "1.0.0",
        "server": { "type": "binary", "entry_point": "server/x",
            "mcp_config": { "command": "${__dirname}/server/x" } },
        "user_config": {
            "toasts": { "type": "string", "title": "Toasts", "default": "true" },
            "token":  { "type": "string", "sensitive": true, "required": true },
            "retries": { "type": "number", "min": 0, "max": 5 },
            "verbose": { "type": "boolean" }
        }
    }"#;

    struct Fx {
        dir: PathBuf,
        servers: PathBuf,
        registry: PathBuf,
    }

    fn fixture(label: &str) -> Fx {
        let dir = std::env::temp_dir().join(format!("uia-cfg-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let servers = dir.join("mcp-local-servers");
        std::fs::create_dir_all(servers.join("mymy")).unwrap();
        std::fs::write(servers.join("mymy").join("manifest.json"), MANIFEST).unwrap();
        let registry = dir.join("uia-mcp.json");
        let mut r = McpRegistry::default();
        r.add_local_server("mymy".into(), Some("1.0.0".into()))
            .unwrap();
        save(&registry, &r).unwrap();
        Fx {
            dir,
            servers,
            registry,
        }
    }

    fn changes(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
            .collect()
    }

    #[test]
    fn path_vars_carry_the_separators_and_the_home_directory() {
        let vars = mcp_path_vars();
        assert_eq!(
            vars.get("/").map(String::as_str),
            Some(std::path::MAIN_SEPARATOR_STR)
        );
        assert_eq!(
            vars.get("pathSeparator").map(String::as_str),
            Some(std::path::MAIN_SEPARATOR_STR)
        );
        if let Some(dirs) = directories::UserDirs::new() {
            assert_eq!(
                vars.get("HOME").map(String::as_str),
                Some(dirs.home_dir().to_string_lossy().as_ref())
            );
        }
    }

    #[test]
    fn the_settings_form_shows_the_default_with_path_variables_expanded() {
        let fx = fixture("pathdefault");
        std::fs::write(
            fx.servers.join("mymy").join("manifest.json"),
            r#"{"manifest_version":"0.3","name":"mymy","version":"1",
                "server":{"type":"binary","entry_point":"server/mymy"},
                "user_config":{"root":{"type":"directory","default":"${HOME}/x"}}}"#,
        )
        .unwrap();
        let secrets = FakeSecretStore::new();
        let view =
            describe_local_config(&load(&fx.registry), &fx.servers, &secrets, "mymy").unwrap();
        std::fs::remove_dir_all(&fx.dir).ok();
        let Some(home) = mcp_path_vars().get("HOME").cloned() else {
            return;
        };
        assert_eq!(view[0].default, Some(format!("{home}/x")));
    }

    #[test]
    fn an_old_registry_file_without_user_config_still_loads() {
        let raw =
            r#"{"local_servers":[{"name":"a","version":"1","enabled":true}],"remote_servers":[]}"#;
        let r: McpRegistry = serde_json::from_str(raw).unwrap();
        assert!(r.local_servers[0].user_config.is_empty());
    }

    #[test]
    fn plain_values_land_in_the_registry_and_secrets_land_only_in_the_keyring() {
        let fx = fixture("split");
        let secrets = FakeSecretStore::new();
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[
                ("toasts", Some("false")),
                ("token", Some("sekret-123")),
                ("retries", Some("3")),
            ]),
        )
        .unwrap();

        let file = std::fs::read_to_string(&fx.registry).unwrap();
        assert!(file.contains("\"toasts\""), "{file}");
        assert!(
            !file.contains("sekret-123"),
            "secret leaked into the registry: {file}"
        );
        assert_eq!(
            secrets.get("mcp-config.mymy.token").as_deref(),
            Some("sekret-123")
        );

        let view =
            describe_local_config(&load(&fx.registry), &fx.servers, &secrets, "mymy").unwrap();
        let token = view.iter().find(|f| f.key == "token").unwrap();
        assert!(token.is_set);
        assert_eq!(
            token.value, None,
            "a sensitive value must never be returned"
        );
        let toasts = view.iter().find(|f| f.key == "toasts").unwrap();
        assert_eq!(toasts.value.as_deref(), Some("false"));
        assert_eq!(toasts.default.as_deref(), Some("true"));
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn a_key_the_manifest_does_not_declare_is_rejected_and_nothing_is_written() {
        let fx = fixture("undeclared");
        let secrets = FakeSecretStore::new();
        let before = std::fs::read_to_string(&fx.registry).unwrap();
        let err = save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("token", Some("t")), ("nope", Some("x"))]),
        )
        .unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert_eq!(std::fs::read_to_string(&fx.registry).unwrap(), before);
        assert_eq!(
            secrets.get("mcp-config.mymy.token"),
            None,
            "secret written despite rejection"
        );
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn values_are_checked_against_their_declared_type_and_range() {
        let fx = fixture("types");
        let secrets = FakeSecretStore::new();
        for (key, bad) in [("retries", "9"), ("retries", "abc"), ("verbose", "yes")] {
            let err = save_local_config(
                &fx.registry,
                &fx.servers,
                &secrets,
                "mymy",
                changes(&[("token", Some("t")), (key, Some(bad))]),
            )
            .unwrap_err();
            assert!(err.contains(key), "{key}={bad}: {err}");
        }
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn saving_without_a_required_value_is_refused() {
        let fx = fixture("required");
        let secrets = FakeSecretStore::new();
        let err = save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("toasts", Some("true"))]),
        )
        .unwrap_err();
        assert!(err.contains("token"), "{err}");
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn an_omitted_secret_is_kept_and_a_null_secret_is_cleared() {
        let fx = fixture("keep");
        let secrets = FakeSecretStore::new();
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("token", Some("t1"))]),
        )
        .unwrap();
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("toasts", Some("false"))]),
        )
        .unwrap();
        assert_eq!(secrets.get("mcp-config.mymy.token").as_deref(), Some("t1"));
        // Clearing a required secret is refused (it has no default)...
        assert!(
            save_local_config(
                &fx.registry,
                &fx.servers,
                &secrets,
                "mymy",
                changes(&[("token", None)])
            )
            .is_err()
        );
        assert_eq!(secrets.get("mcp-config.mymy.token").as_deref(), Some("t1"));
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn local_config_values_reads_plain_from_the_entry_and_secrets_from_the_keyring() {
        let fx = fixture("values");
        let secrets = FakeSecretStore::new();
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("token", Some("t")), ("toasts", Some("false"))]),
        )
        .unwrap();
        let reg = load(&fx.registry);
        let fields = uia_mcp::bundle::parse_manifest(MANIFEST)
            .unwrap()
            .user_config;
        let v = local_config_values(&reg.local_servers[0], &fields, &secrets);
        assert_eq!(v.get("token").map(String::as_str), Some("t"));
        assert_eq!(v.get("toasts").map(String::as_str), Some("false"));
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn removing_a_server_clears_its_keyring_entries_and_its_files() {
        let fx = fixture("remove");
        let secrets = FakeSecretStore::new();
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("token", Some("t"))]),
        )
        .unwrap();
        remove_local_server_and_clear_config(&fx.registry, &fx.servers, &secrets, "mymy").unwrap();
        assert!(load(&fx.registry).local_servers.is_empty());
        assert_eq!(secrets.get("mcp-config.mymy.token"), None);
        assert!(!fx.servers.join("mymy").exists());
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    const MANIFEST2: &str = r#"{
        "manifest_version": "0.3", "name": "mymy", "version": "1.0.0",
        "server": { "type": "binary", "entry_point": "server/x",
            "mcp_config": { "command": "${__dirname}/server/x" } },
        "user_config": {
            "a": { "type": "string", "sensitive": true },
            "b": { "type": "string", "sensitive": true },
            "note": { "type": "string" },
            "region": { "type": "string", "required": true, "default": "eu" }
        }
    }"#;

    fn fixture2(label: &str) -> Fx {
        let fx = fixture(label);
        std::fs::write(fx.servers.join("mymy").join("manifest.json"), MANIFEST2).unwrap();
        fx
    }

    /// Delegates to a `FakeSecretStore` but fails the Nth `set` (1-based) and,
    /// optionally, every `delete` of one account.
    struct FailingStore {
        inner: FakeSecretStore,
        sets: std::sync::atomic::AtomicUsize,
        fail_set_on_nth: usize,
        fail_delete_of: Option<String>,
        /// Like the real OS keyring: deleting an absent entry is an error.
        strict_delete: bool,
    }

    impl FailingStore {
        fn new(fail_set_on_nth: usize, fail_delete_of: Option<&str>) -> Self {
            Self {
                inner: FakeSecretStore::new(),
                sets: std::sync::atomic::AtomicUsize::new(0),
                fail_set_on_nth,
                fail_delete_of: fail_delete_of.map(str::to_string),
                strict_delete: false,
            }
        }
    }

    impl SecretStore for FailingStore {
        fn get(&self, account: &str) -> Option<String> {
            self.inner.get(account)
        }
        fn set(&self, account: &str, value: &str) -> Result<(), crate::secrets::SecretStoreError> {
            let n = self.sets.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n == self.fail_set_on_nth {
                return Err(crate::secrets::SecretStoreError::Backend("boom".into()));
            }
            self.inner.set(account, value)
        }
        fn delete(&self, account: &str) -> Result<(), crate::secrets::SecretStoreError> {
            if self.fail_delete_of.as_deref() == Some(account) {
                return Err(crate::secrets::SecretStoreError::Backend(
                    "no delete".into(),
                ));
            }
            if self.strict_delete && self.inner.get(account).is_none() {
                return Err(crate::secrets::SecretStoreError::Backend(
                    "no such entry".into(),
                ));
            }
            self.inner.delete(account)
        }
    }

    #[test]
    fn a_failing_second_secret_write_restores_the_first_to_unset_and_leaves_the_registry_alone() {
        let fx = fixture2("rb-unset");
        let secrets = FailingStore::new(2, None);
        let before = std::fs::read_to_string(&fx.registry).unwrap();
        let err = save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("a", Some("A2")), ("b", Some("B2")), ("note", Some("n"))]),
        )
        .unwrap_err();
        assert!(err.contains("boom"), "{err}");
        assert_eq!(secrets.get("mcp-config.mymy.a"), None);
        assert_eq!(secrets.get("mcp-config.mymy.b"), None);
        assert_eq!(std::fs::read_to_string(&fx.registry).unwrap(), before);
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn a_failing_second_secret_write_restores_the_first_to_its_prior_value() {
        let fx = fixture2("rb-prior");
        // fail on the 3rd set: the first save's `a` is set #1, this save's `a` #2, `b` #3.
        let secrets = FailingStore::new(3, None);
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("a", Some("A1"))]),
        )
        .unwrap();
        let before = std::fs::read_to_string(&fx.registry).unwrap();
        let err = save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("a", Some("A2")), ("b", Some("B2")), ("note", Some("n"))]),
        )
        .unwrap_err();
        assert!(err.contains("boom"), "{err}");
        assert_eq!(secrets.get("mcp-config.mymy.a").as_deref(), Some("A1"));
        assert_eq!(secrets.get("mcp-config.mymy.b"), None);
        assert_eq!(std::fs::read_to_string(&fx.registry).unwrap(), before);
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn a_failing_delete_is_surfaced_and_earlier_changes_are_restored() {
        let fx = fixture2("rb-delete");
        let secrets = FailingStore::new(usize::MAX, Some("mcp-config.mymy.b"));
        secrets.inner.set("mcp-config.mymy.a", "A1").unwrap();
        secrets.inner.set("mcp-config.mymy.b", "B1").unwrap();
        let err = save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("a", Some("A2")), ("b", None)]),
        )
        .unwrap_err();
        assert!(err.contains("no delete"), "{err}");
        assert_eq!(secrets.get("mcp-config.mymy.a").as_deref(), Some("A1"));
        assert_eq!(secrets.get("mcp-config.mymy.b").as_deref(), Some("B1"));
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_registry_write_restores_the_keyring() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture2("rb-registry");
        let secrets = FakeSecretStore::new();
        secrets.set("mcp-config.mymy.a", "A1").unwrap();
        let before = std::fs::read_to_string(&fx.registry).unwrap();
        std::fs::set_permissions(&fx.registry, std::fs::Permissions::from_mode(0o444)).unwrap();
        if std::fs::OpenOptions::new()
            .append(true)
            .open(&fx.registry)
            .is_ok()
        {
            // Privileged enough (root) to ignore the mode: the write cannot be
            // made to fail this way, so there is nothing to prove.
            std::fs::remove_dir_all(&fx.dir).ok();
            return;
        }
        let err = save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("a", Some("A2")), ("b", Some("B2")), ("note", Some("n"))]),
        )
        .unwrap_err();
        std::fs::set_permissions(&fx.registry, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!err.is_empty());
        assert_eq!(secrets.get("mcp-config.mymy.a").as_deref(), Some("A1"));
        assert_eq!(secrets.get("mcp-config.mymy.b"), None);
        assert_eq!(std::fs::read_to_string(&fx.registry).unwrap(), before);
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn clearing_a_non_required_secret_deletes_it_and_clearing_a_plain_value_removes_it() {
        let fx = fixture2("clear");
        let secrets = FakeSecretStore::new();
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("a", Some("A1")), ("note", Some("n"))]),
        )
        .unwrap();
        assert_eq!(secrets.get("mcp-config.mymy.a").as_deref(), Some("A1"));
        assert_eq!(
            load(&fx.registry).local_servers[0]
                .user_config
                .get("note")
                .map(String::as_str),
            Some("n")
        );
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("a", None), ("note", Some(""))]),
        )
        .unwrap();
        assert_eq!(secrets.get("mcp-config.mymy.a"), None);
        assert!(load(&fx.registry).local_servers[0].user_config.is_empty());
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn non_finite_numbers_are_rejected_and_the_range_bounds_are_inclusive() {
        let fx = fixture("numbers");
        let secrets = FakeSecretStore::new();
        for bad in ["NaN", "inf", "-inf", "infinity"] {
            let err = save_local_config(
                &fx.registry,
                &fx.servers,
                &secrets,
                "mymy",
                changes(&[("token", Some("t")), ("retries", Some(bad))]),
            )
            .unwrap_err();
            assert!(err.contains("retries"), "{bad}: {err}");
        }
        for ok in ["0", "5"] {
            save_local_config(
                &fx.registry,
                &fx.servers,
                &secrets,
                "mymy",
                changes(&[("token", Some("t")), ("retries", Some(ok))]),
            )
            .unwrap();
        }
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn a_required_field_with_a_default_needs_no_value() {
        let fx = fixture2("default");
        let secrets = FakeSecretStore::new();
        save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("note", Some("n"))]),
        )
        .unwrap();
        std::fs::remove_dir_all(&fx.dir).ok();
    }

    #[test]
    fn rolling_back_never_deletes_an_entry_that_was_never_written() {
        let fx = fixture2("rb-strict");
        let mut secrets = FailingStore::new(2, None);
        secrets.strict_delete = true;
        let before = std::fs::read_to_string(&fx.registry).unwrap();
        let err = save_local_config(
            &fx.registry,
            &fx.servers,
            &secrets,
            "mymy",
            changes(&[("a", Some("A2")), ("b", Some("B2"))]),
        )
        .unwrap_err();
        assert!(err.contains("boom"), "{err}");
        assert!(
            !err.contains("restor"),
            "a spurious rollback failure was reported: {err}"
        );
        assert_eq!(secrets.get("mcp-config.mymy.a"), None);
        assert_eq!(secrets.get("mcp-config.mymy.b"), None);
        assert_eq!(std::fs::read_to_string(&fx.registry).unwrap(), before);
        std::fs::remove_dir_all(&fx.dir).ok();
    }
}
