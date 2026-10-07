# MCPB `user_config` support

## Problem

A `.mcpb` manifest can declare a `user_config` block: settings the user fills
in once, referenced from `mcp_config` as `${user_config.<key>}`. Claude Desktop
renders a form from it and substitutes the values at launch.

UIA ignores the block. `McpbManifest` drops it, `resolve_launch` substitutes
only `${__dirname}`, and Settings → MCP has nothing to show. The installed
`mymy-assistant` bundle declares `enable_system_toasts` and passes it as
`ENABLE_SYSTEM_TOASTS="${user_config.enable_system_toasts}"`. UIA launches the
server with that literal text, and the user has no way to change it.

## Goal and success criteria

- Settings → MCP shows a **Configuration** section for any installed local
  server whose manifest declares `user_config`.
- The saved values are substituted into `command`, `args` and `env` at launch.
- `mymy-assistant` shows "Desktop toast notifications", and the chosen value
  reaches the server as `ENABLE_SYSTEM_TOASTS`.
- Sensitive values never touch `uia-mcp.json` or the frontend.
- A required field with no value and no default makes the server show as
  **Failed** with a reason; it is not launched with literal `${...}` text.

Assumption: changes apply on the next restart, like every other MCP registry
change (`docs/MCP.md`, "Status").

## Design

### 1. Parsing — `uia-mcp/src/bundle/manifest.rs`

`McpbManifest` gains `user_config: BTreeMap<String, UserConfigField>`
(`#[serde(default)]`). `UserConfigField` carries `type`, `title`,
`description`, `required`, `default`, `sensitive`, and `min`/`max`/`multiple`
as declared by the MCPB schema. Supported types: `string`, `number`,
`boolean`, `directory`, `file`. Unknown keys are ignored, as for the rest of
the manifest, so an unfamiliar field never makes a bundle uninstallable.

### 2. Substitution — `uia-mcp`

A new `apply_user_config(launch, fields, values)` takes the user's values
(`BTreeMap<String, String>`) and replaces `${user_config.<key>}` in `args` and
`env` at launch. It runs after `validate_bundle`; `resolve_launch` and
`validate_bundle` are unchanged in how they treat templates.

`${user_config.*}` in the launch `command` is unsupported and refused at
validation (error `UserConfigInCommand`) — a setting can never choose what
executable runs.

Precedence: stored value, then manifest default, then an error when the field
is `required`, else an empty string. Booleans render as `"true"`/`"false"`,
numbers in their plain decimal form. A `multiple` field is parsed but
substituted as its first value only; joining several values is out of scope.

Order and safety:

- `validate_bundle` runs on the manifest with `${user_config.*}` left in
  place, exactly as today. The four trust checks therefore judge what the
  bundle declares, not what a user typed.
- Substitution happens after validation, in one pass. A value containing
  `${__dirname}` or `${user_config.x}` is not expanded again.
- A new `BundleError::MissingUserConfig(key)` is the required-and-unset case.
  `mcp_targets` reports it in its skip reasons, so the Status column shows
  Failed with that sentence.

### 3. Storage — `uia-app`

`LocalServerEntry` gains `user_config: BTreeMap<String, String>`
(`#[serde(default)]`; existing `uia-mcp.json` files load unchanged). It holds
only non-sensitive values.

Sensitive fields go to the OS keyring under accounts
`mcp-config.<server>.<key>`. `secrets.rs` gains a
`MCP_CONFIG_ACCOUNT_PREFIX` and `is_mcp_config_account`, wired into
`is_known_account`, mirroring `mcp-oauth.`. Both `<server>` and `<key>` must
pass the existing `[A-Za-z0-9_-]` rule, so frontend-controlled input cannot
reach an arbitrary account name.

"Keyring" is the existing `OsKeyring` store, so no new dependency or backend is
needed: macOS Keychain (`keyring/apple-native`), Windows Credential Manager
(`keyring/windows-native`), and Secret Service on Linux
(`crates/uia-app/Cargo.toml`). macOS and Windows are the primary targets.

`remove_local_server` deletes the server's keyring entries along with its
record, in the same order as `remove_remote_and_clear_credentials`: record
first, credentials second.

### 4. IPC — `main.rs` adapters over `mcp_registry.rs` logic

- `get_mcp_local_server_config(name)` returns the field schema plus current
  values. A sensitive field returns `{ set: bool }`, never the value.
- `set_mcp_local_server_config(name, values)` validates against the manifest
  schema (type, `min`/`max`, required), writes non-sensitive values to the
  registry and sensitive ones to the keyring, and rejects keys the manifest
  does not declare. An omitted sensitive field keeps its stored value; an
  explicit clear deletes it.

As with `describe_local_server`, the logic lives in `mcp_registry.rs` so it is
testable without the `desktop` feature; the commands are thin adapters.
`mcp_targets` and `describe_local_server` load stored values (keyring
included, via an injected `SecretStore`) and pass them to the launcher.
`describe_local_server` (the Details panel) is unchanged and shows the
validated launch with `${user_config.*}` unresolved, so the panel still works
while a required setting is missing — that is where the form lives.
`LocalServerDetails.env_keys` is unchanged; resolved values are still never
returned.

### 5. UI — `src/SettingsMcp.svelte`

In a local server's expanded Details panel, a **Configuration** section
appears only when the manifest declares fields. `boolean` renders a toggle;
`string` and `number` render inputs (`sensitive` renders a password input
with a "set" indicator); `directory` and `file` use the existing
`@tauri-apps/plugin-dialog` picker. Saving shows the same restart-required
notice the rest of the tab uses. `mymy-assistant` declares
`enable_system_toasts` as a `string`, so it renders as a text input; giving
that one field a toggle is the bundle author's change to make.

## Error handling

- Missing or unreadable manifest while loading the form: the section shows the
  same reason `describe_local_server` already gives.
- Invalid value on save: the command returns a field-specific message; nothing
  is written (all-or-nothing, as for install).
- Keyring unavailable on save of a sensitive value: the save fails with the
  `SecretStoreError` message and the registry is not updated.
- Keyring unavailable at launch: a required sensitive field reads as unset, so
  the server is Failed with the missing-field reason rather than starting
  without it.

## Testing

- Manifest: `user_config` parses, including the real `mymy-assistant`
  manifest as a fixture; a manifest without it still parses.
- Substitution: stored value, default, required-missing, boolean and number
  rendering, platform-override merging, and a value containing `${...}` is not
  re-expanded.
- Validation: a `user_config` value cannot change the validated command. A
  test pins what `validate_bundle` does with `${user_config.x}` as the whole
  `command`; if it is not rejected today, the implementation must reject it
  rather than let a user-set value choose the executable.
- Registry/secrets (with `FakeSecretStore`): sensitive values land in the
  keyring and not in `uia-mcp.json`; undeclared keys are rejected; removing a
  server clears its keyring entries; `is_known_account` accepts
  `mcp-config.<server>.<key>` and rejects traversal-shaped names.
- `mcp_targets`: a required unset field is skipped with the expected reason.
- Frontend: no existing test harness for the Settings components, so the
  change is checked by `pnpm build` and `svelte-check`.

## Out of scope

Live reload without restart; a "test connection" button; editing the manifest;
`multiple`-valued fields beyond the first value; converting a `string` field
into a toggle on the host's initiative.
