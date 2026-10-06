# OS-Native Secrets Storage — Design

## Context

This is sub-project 1 of a larger F.R.I.D.A.Y. SP2 UI/engine expansion (Microsoft
Foundry as a third engine, a settings tab, main-tab UI overhaul, window
close-to-tray behavior). It was split out because sub-project 3 (settings UI)
needs a working secrets store before it can wire up API-key input, and the
Foundry engine (sub-project 2) should ship its credentials through the same
mechanism from day one rather than bolting it on after.

Today, `crates/uia-app/src/config.rs` resolves credentials with this
precedence, implemented separately in `OpenAiSection::resolve_api_key()`
(line 486) and `NovaSection::resolve_credentials()` (line 561):

1. Environment variable (`OPENAI_API_KEY`, `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`)
2. A key file path (`key_file` field, read and trimmed)
3. An inline value in `uia.toml` (`api_key`/`access_key_id`/`secret_access_key`)

All three of those can leave a real secret sitting in plaintext on disk. This
design adds a fourth, higher-priority tier: the OS-native credential store
(Windows Credential Manager / macOS Keychain / Linux Secret Service), via the
`keyring` crate.

## Goals

- Real secrets, once entered through the app, live in the OS-native store —
  not in `uia.toml`, not in a key file.
- Existing plaintext credentials (TOML inline value or key file) keep working
  as a fallback — this must not break CI, headless use, or anyone who hasn't
  touched the new UI yet.
- Existing plaintext credentials are opportunistically migrated into the OS
  store on first load, without deleting the plaintext source (so migration is
  reversible by deleting the OS entry).
- The Tauri command surface for writing/clearing secrets exists now, so the
  settings-UI sub-project only has to call it — no backend work left there.
- The frontend (Svelte) never reads a secret value back. Write-only from the
  UI, matching the architectural rule that the webview never sees credentials
  or audio frames.
- `cargo test --workspace` stays offline and credential-free.

## Non-goals

- No settings UI in this sub-project (sub-project 3).
- No Foundry engine wiring in this sub-project (sub-project 2) — the
  `FoundrySection` config struct is added here as plumbing, but Foundry's own
  `connect()` protocol work is out of scope.
- No secret rotation, expiry, or multi-account support. One value per
  account, latest write wins.

## Architecture

### Where this lives

`keyring` is a platform I/O crate, so it follows the same rule that already
keeps `cpal` and `tauri` out of `uia-core`
(`scripts/check-core-deps.sh`). It is added only to `uia-app`. The
existing `check-core-deps.sh` forbidden-crate list gains `keyring` for the
same reason cpal/tauri/webrtc are already there.

### `SecretStore`

New module `crates/uia-app/src/secrets.rs`:

```rust
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Option<String>;
    fn set(&self, account: &str, value: &str) -> Result<(), SecretStoreError>;
    fn delete(&self, account: &str) -> Result<(), SecretStoreError>;
}

pub struct OsKeyring;   // wraps the `keyring` crate, service name "uia"
pub struct FakeSecretStore { .. } // in-memory, used by tests and by the
                                    // credential-resolution unit tests below
```

`account` values used throughout the app: `"openai_api_key"`,
`"nova_access_key_id"`, `"nova_secret_access_key"`, `"foundry_api_key"`.

`OsKeyring::get`/`set`/`delete` wrap `keyring::Entry::new("uia", account)`
and its `.get_password()` / `.set_password()` / `.delete_credential()`.
Platform errors (no backend available, permission denied) are logged and
treated as "absent" for `get` — a broken OS keystore must degrade to the
existing fallback chain, not crash the app.

### Credential resolution

`OpenAiSection::resolve_api_key(&self, store: &dyn SecretStore)` and
`NovaSection::resolve_credentials(&self, store: &dyn SecretStore)` gain a
new first-checked tier ahead of the existing three:

1. **OS keystore** (`store.get("openai_api_key")`, etc.)
2. Environment variable (unchanged)
3. Key file (unchanged)
4. Inline TOML value (unchanged)

Both functions take `&dyn SecretStore` as a parameter (not a global) so
existing tests keep passing an in-memory `FakeSecretStore` — no test needs
real OS keychain access, matching every other config test's offline pattern.

A new `FoundrySection` struct is added to `config.rs`, structurally identical
to `OpenAiSection` (`api_key: Option<String>`, `key_file: Option<String>`,
`endpoint: Option<String>`), with `resolve_api_key(&self, store: &dyn
SecretStore)` following the identical four-tier precedence. `endpoint` has no
OS-store tier — it's not a secret. `Config` gains a `foundry:
FoundrySection` field, `#[serde(default)]` so existing `uia.toml` files
without a `[foundry]` section keep parsing unchanged.

### Migration on load

`Config::load` (or the call site that constructs the real `OsKeyring` and
calls `resolve_*`) runs, for each of the four accounts: if
`store.get(account)` is `None` and the corresponding env/file/inline
resolution succeeds, call `store.set(account, resolved_value)` and log
`info!("migrated {account} into OS keystore")`. This is opportunistic and
non-destructive — the TOML/key-file value is left in place. A migration
failure (e.g. OS keystore unavailable) is logged as a warning and the app
proceeds using the plaintext value for that session, matching the "degrade
to fallback chain" rule above.

Migration only runs once per account per process start, driven by the
existing config-load path — not a background task, not on every
`resolve_*` call.

### Tauri commands

Added to `crates/uia-app/src/main.rs` alongside `set_engine`/`get_engine`:

```rust
#[tauri::command]
fn get_secret_status(account: String) -> bool
// true if a value is present via ANY tier (keystore or fallback) — the UI
// needs "is this configured", never the value itself.

#[tauri::command]
fn set_secret(account: String, value: String) -> Result<(), String>
// writes directly to the OS keystore via SecretStore::set.

#[tauri::command]
fn delete_secret(account: String) -> Result<(), String>
// SecretStore::delete. Does not touch TOML/key-file fallback values —
// deleting those remains a manual file edit, out of scope here.
```

`account` is validated against the fixed set of four known strings; anything
else returns `Err("unknown secret account: {account}")` rather than writing
an arbitrary keyring entry.

These three commands are registered in `invoke_handler!` now. No UI calls
them yet — that's sub-project 3's job — but the backend surface is complete.

### Error handling

- OS keystore unavailable/misconfigured at runtime → `SecretStoreError`,
  logged, treated as "no value" by `get` (triggers fallback chain) and
  surfaced as a `Result::Err` string to the frontend for `set`/`delete` (the
  future UI will show this as a toast/error state).
- Unknown `account` string on any Tauri command → `Err`, never a panic.

### Testing

- `secrets.rs`: unit tests for `FakeSecretStore` (get/set/delete round-trip,
  `get` on missing account returns `None`) and for `OsKeyring` gated behind
  `#[ignore]` (needs a real OS keystore, same pattern as SP1's live engine
  tests) so `cargo test --workspace` never touches the real keychain.
- `config.rs`: existing precedence tests (`env_var_wins_over_key_file...`,
  etc.) are updated to pass a `FakeSecretStore` and gain new tests: "keystore
  value wins over env var", "keystore value wins over inline TOML", "absent
  keystore value falls through to existing chain unchanged".
- `config.rs`: new migration tests — "plaintext value with empty keystore
  gets migrated", "existing keystore value is never overwritten by
  migration", using `FakeSecretStore` to assert on the post-migration state.
- `main.rs`: unit tests for the three new Tauri commands' account-validation
  logic (reject unknown account strings) using the same pattern as the
  existing `set_engine` test, if one exists — otherwise a new small test
  module.

## Open questions resolved during brainstorming

- Library: `keyring` crate (not raw per-OS APIs).
- Fallback scope: keystore-first, old methods remain as fallback (not
  removed).
- Migration: automatic, opportunistic, non-destructive.
- Command surface: included in this sub-project, not deferred to the UI
  sub-project.
