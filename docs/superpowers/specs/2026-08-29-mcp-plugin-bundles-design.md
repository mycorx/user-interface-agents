# MCP server management (.mcpb local plugins + gated remote servers)

## Problem

MCP transport, routing, and tool translation already exist and work
(`crates/uia-mcp`: `McpExecutor`, `McpRouter`, `translate`), and
`uia-app` already connects whatever `[[mcp.servers]]` entries appear in
`uia.toml` at session startup (`session.rs::build_mcp_router`). Gaps:

1. There is no UI for managing MCP servers — only hand-editing
   `uia.toml` and restarting.
2. Local (stdio) servers have no install step or validation: a
   `command`/`args` pair can point at anything, with no record of what
   was trusted to run or why.
3. Remote (HTTP) servers are legitimate and wanted (Claude Code
   supports `claude mcp add --transport http`), but the app should let
   the user consciously turn that surface on rather than have it live
   by default next to trusted local plugins.

This spec adds a Settings tab, modeled on Claude Code's `mcp add`
experience, with two ways to get a server: install a `.mcpb` bundle for
local stdio servers (validated, binary-only), or manually add a remote
HTTP server (name + URL + optional auth header) gated behind an
explicit "allow remote servers" switch.

## Non-goals

- Live add/remove of a server mid-session (servers still connect once
  at startup, matching every other Settings tab's restart-to-apply
  model).
- Per-tool confirmation/permission gating (`requires_confirmation`
  stays `false`, unchanged from today).
- A plugin marketplace/registry/discovery UI. Local install is a file
  picker pointed at a `.mcpb` the user already has; remote add is a
  manual form, not a directory of known servers.
- SSE transport. Claude Code still lists it, but treats it as the
  deprecated predecessor to streamable HTTP; `uia-mcp` already only
  implements `transport-streamable-http-client-reqwest`, so remote
  stays HTTP-only.

## Trust model

Two kinds of registry entry, both starting **disabled** on creation so
enabling one is always a deliberate second step (matches Claude Code's
own "added but review before trusting" posture):

**Local plugin (`.mcpb`).** A zip containing `manifest.json` (the real
MCPB/DXT schema: `dxt_version`, `name`, `version`, `server.type`,
`server.entry_point`, `server.mcp_config`, optional
`server.mcp_config.platform_overrides`). Accepted only if, at install
time:

- `server.type == "binary"` — `"node"` and `"python"` bundles (which
  imply a bundled interpreter running bundled source) are rejected
  outright, with the manifest's declared type in the error.
- The resolved entry point's extension is not in a script denylist:
  `.py .js .mjs .cjs .ts .rb .sh .bash .ps1 .bat .cmd .php .pl`.
- The resolved entry point file does not begin with a `#!` shebang
  line (belt-and-suspenders for an extensionless script).
- The resolved entry point exists inside the extracted bundle (no
  path escaping `../`).

"Resolved" accounts for `platform_overrides` picking a different entry
point per OS. Any failure aborts the install before extraction is
kept — nothing partially-trusted is left on disk.

**Remote server (HTTP).** Name + URL, entered by hand, plus an
optional single auth header (`header name` + `value`, e.g.
`Authorization: Bearer …`) — the same shape as Claude Code's `-H` flag
on `mcp add`. No bundle, no code runs locally; the risk is "the app
will send requests to this URL with this header."

Unlike a local bundle, a remote server has no file to inspect ahead of
time — so the declaration comes from the MCP protocol itself. Every
compliant server answers the standard `initialize` handshake with
`serverInfo` (name, version) and, on `tools/list`, its full tool set
with descriptions and input schemas. This is exactly what Claude Code
shows when you `claude mcp add` a remote server before it's trusted,
and it's what `uia-mcp` already parses on every connect
(`McpExecutor::connect` + `list_tools`) — nothing new needs to be
fetched, only surfaced and persisted before the entry is saved rather
than discarded after populating the live tool list.

Adding a remote server is therefore two steps, not one form
submission: the app connects live to the given URL/header, shows the
returned server name/version and tool list as a preview, and only
*then* does confirming persist the entry (disabled by default, same as
local plugins) — together with a snapshot of that declaration so the
Settings list can show "what this is and what it can do" without
reconnecting every time the tab opens. A URL that fails to answer
`initialize`, or answers with zero tools, is refused at add time —
there is nothing to declare, so there is nothing to trust.

This stored snapshot is for the Settings UI's own review, not for the
agent: the tool declarations the model actually receives always come
from `McpRouter::list_tools()` re-querying every enabled server fresh
at session start (unchanged by this spec — the same path a local
`.mcpb` plugin's tools already go through), then
`translate::to_nova_declaration`/`to_openai_declaration`. The agent
never reasons from the cached preview, so a stale snapshot can only
mislead a human reading Settings, never the model's own tool list.

Remote servers are further gated by one global switch, **"Allow remote
MCP servers"**, default **off**. While off, no remote entry connects
regardless of its own enabled flag — the per-server flag says "trust
this one," the global switch says "trust remote at all." Both live in
the same registry file so one restart-gated view covers everything.

Header values are secrets-adjacent; store them the way `config.rs`
already treats API keys (plain in the local registry file, never
logged, never round-tripped into any error message).

## Architecture

### `crates/uia-mcp`: new `plugin` module (local bundles only)

- `McpbManifest` — deserialized `manifest.json` subset: `name`,
  `version`, `server.type`, `server.entry_point`,
  `server.mcp_config.command` (fallback when `entry_point` isn't
  directly executable), `server.mcp_config.args`,
  `server.mcp_config.env`, `server.mcp_config.platform_overrides`.
- `validate_manifest(&McpbManifest, current_os) -> Result<ResolvedEntry, PluginError>`
  — the denylist/shebang/type checks above, pure and unit-testable
  without touching the filesystem beyond reading the one entry-point
  file.
- `install_bundle(source: &Path, dest_dir: &Path) -> Result<InstalledPlugin, PluginError>`
  — opens the zip (new `zip` workspace dependency), extracts to a temp
  dir, validates, and only then moves the temp dir into
  `dest_dir/<name>/`. Returns the plugin's name, version, and the
  resolved absolute entry-point path.
- `McpServerConfig::Stdio` gains an `env: Vec<(String, String)>` field
  (additive; a bundle's `mcp_config.env` is part of what the manifest
  declares). `McpServerConfig::Http` **stays** — it's the transport a
  remote registry entry converts to — and gains an optional
  `headers: Vec<(String, String)>` field so `McpExecutor::connect`'s
  `Http` arm can attach the auth header before calling
  `StreamableHttpClientTransport::from_uri`.
- `McpExecutor` gains `pub fn declaration(&self) -> ServerDeclaration`
  (`{ name: String, version: Option<String> }` from the `initialize`
  response's `serverInfo`, already available on the connected
  `RunningService` after `connect` succeeds) so a caller can capture
  "what this server says it is" using the same connection it's about
  to call `list_tools` on, instead of a second round trip.

### `crates/uia-app`: registry + Tauri commands

- `paths.rs` gains `mcp_plugins_dir()` alongside the existing
  `config_dir()`-style resolvers: `<config_dir>/mcp-plugins/` (local
  bundle extraction target).
- New `mcp_registry.rs`: `McpRegistry` — a `registry.json` in
  `config_dir()` with:
  - `allow_remote: bool` (default `false`)
  - `plugins: Vec<{name, version, enabled}>` (local, `.mcpb`-sourced)
  - `remote_servers: Vec<{name, url, header_name, header_value, enabled, declared_name, declared_version, tools: Vec<{name, description}>}>`
    — `declared_*`/`tools` are the `initialize`/`tools/list` snapshot
    captured at add time, refreshed whenever the user re-adds or a
    future "recheck" action re-runs the same preview (no such action
    in this pass; see open items)

  Loaded/saved with the same read-whole-file-write-whole-file approach
  `settings.rs` already uses for `AgentSettings`.
- `config.rs`: delete `McpSection`, `McpServer`, the `[[mcp.servers]]`
  parsing, `Config::mcp`, and the now-dead
  `mcp_servers_parse_for_both_transports` /
  `an_mcp_server_name_that_would_disable_every_tool_is_rejected_at_load`
  tests. The name-validation guarantee they exercise moves to
  `mcp_registry.rs` (local names) and stays enforced on remote names
  too, since `uia_mcp::validate_server_name` is transport-agnostic
  already.
- `session.rs`: `mcp_targets`/`build_mcp_router` build their target
  list from `McpRegistry` instead of `config.mcp.servers`: every
  enabled local plugin becomes a `Stdio` target unconditionally;
  every enabled remote entry becomes an `Http` target only if
  `allow_remote` is true (silently skipped otherwise, with an
  `eprintln!` note so a confused user can find out why in logs — the
  same way `McpRouter::list_tools` already logs a skipped dead
  server rather than failing the session).
- New Tauri commands in `main.rs`, following the existing
  `get_agent_settings`/`set_agent_settings` pattern:
  - `list_mcp_plugins() -> Vec<PluginSummary>`
  - `install_mcp_plugin(source_path: String) -> Result<PluginSummary, String>`
  - `remove_mcp_plugin(name: String) -> Result<(), String>`
  - `set_mcp_plugin_enabled(name: String, enabled: bool) -> Result<(), String>`
  - `list_mcp_remote_servers() -> Vec<RemoteServerSummary>` (includes
    the stored `declared_name`/`declared_version`/`tools` snapshot;
    never returns the header value, only whether one is set, so it
    isn't echoed back into the renderer on every load)
  - `preview_mcp_remote_server(name, url, header_name?, header_value?) -> Result<RemoteServerPreview, String>`
    — runs `validate_server_name`, then connects live and returns the
    `initialize`/`tools/list` declaration. Does **not** persist
    anything; a URL that fails to connect or answers with zero tools
    returns an error instead of a preview, so the UI has nothing to
    confirm.
  - `add_mcp_remote_server(name, url, header_name?, header_value?) -> Result<RemoteServerSummary, String>`
    — re-runs the same connect-and-declare step (the preview call is
    read-only and stateless on the backend; nothing is cached between
    the two Tauri calls) and, only on success, persists the entry
    together with its declaration snapshot, disabled by default.
  - `remove_mcp_remote_server(name: String) -> Result<(), String>`
  - `set_mcp_remote_server_enabled(name: String, enabled: bool) -> Result<(), String>`
  - `set_mcp_allow_remote(enabled: bool) -> Result<(), String>`

### `uia.example.toml`

The `[[mcp.servers]]` example block is deleted and replaced with a
short comment pointing at the Settings → MCP tab as the only way to
add servers now.

### Frontend: `src/SettingsMcp.svelte`

New tab, inserted after Providers in `SettingsCard.svelte`'s tab list
(General / Visual / Audio / Providers / MCP). Two sections, following
`SettingsGeneral.svelte`'s structural pattern:

**Local plugins.** List of installed bundles (name, version, enable
checkbox, Remove button with a confirm step since it deletes files).
An "Install…" button opens `@tauri-apps/plugin-dialog`'s `open()`
filtered to `{ name: 'MCP Bundle', extensions: ['mcpb'] }`, then
`invoke('install_mcp_plugin', { sourcePath })`. Validation failures
show inline, same `field-error` pattern `SettingsGeneral.svelte` uses
for `avatarError`.

**Remote servers.** A top-level "Allow remote MCP servers" checkbox
(`set_mcp_allow_remote`). Below it, an "Add server…" form (name, URL,
optional header name/value — collapsed by default, like Claude Code's
`-H` being optional) with a two-step submit: "Connect" calls
`preview_mcp_remote_server` and, on success, replaces the form's submit
area with a read-only capability card (declared server name/version,
and the tool list with descriptions) plus a "Confirm & add" button
that calls `add_mcp_remote_server`; a failed preview shows the error
inline instead (same `field-error` pattern) and never reaches the
confirm step. Added servers list below with their own enable checkbox,
Remove button, and the same capability summary (name/version + tool
count, expandable to the full list) so it stays visible after the
fact, not just at add time. The list and its enable checkboxes render
normally even when the global switch is off, so the user can see and
manage what's configured without it being live — only new connections
at the next startup are gated.

All actions fire the shared `onchange` → restart notice, same as every
other tab's toggles today; nothing here reconnects live.

## Testing

- `uia-mcp::plugin`: unit tests for `validate_manifest` — accepts a
  `"binary"` bundle; rejects `"node"`/`"python"` type; rejects each
  denylisted extension; rejects a shebang'd extensionless entry point;
  rejects a path that escapes the bundle root; resolves the correct
  entry point from `platform_overrides` for the current OS.
- `uia-mcp::plugin`: `install_bundle` integration-style test building a
  minimal zip in-memory (valid binary bundle installs; invalid bundle
  leaves nothing behind in `dest_dir`).
- `uia-mcp::client`: `McpServerConfig::Http` with a header attaches it
  to the request (existing `is_desktop_only` / name-validation tests
  stay; add coverage for the new `headers` field); `McpExecutor::declaration`
  returns the connected server's `serverInfo` name/version.
- `uia-app::mcp_registry`: local plugin enable/disable/remove
  roundtrip; remote server add/enable/disable/remove roundtrip
  including the persisted declaration snapshot; `allow_remote`
  default-off and toggle roundtrip — all against a temp directory,
  same style as `paths.rs`'s existing temp-dir tests.
- `uia-app::main` (Tauri commands): `preview_mcp_remote_server`
  against a fake local MCP server (spawned in-test over stdio or a
  tiny local HTTP mock) returns its declared tools without writing to
  the registry; a URL that never answers `initialize`, or answers with
  zero tools, is rejected; `add_mcp_remote_server` persists only after
  a successful declare.
- `uia-app::session`: `mcp_targets`-equivalent test asserting: an
  enabled local plugin always becomes a `Stdio` target; an enabled
  remote server becomes an `Http` target only when `allow_remote` is
  true, and is skipped (not erroring the whole build) when it's false.
- No frontend test harness exists for any Settings tab today
  (`SettingsGeneral.svelte` etc. are untested); `SettingsMcp.svelte`
  matches that and is verified manually via `npm run tauri dev`
  (install a real `.mcpb`, add a remote server, flip the global
  switch, confirm the restart notice and post-restart connection
  behavior).

## Open items carried forward, not blocking

- Per-tool confirmation UI (`requires_confirmation`) is out of scope
  here and was already out of scope before this change.
- No plugin auto-update / version-check story for local bundles;
  removing and reinstalling a newer `.mcpb` is the upgrade path.
- No bearer-token refresh/OAuth flow for remote servers — a static
  header value only, matching Claude Code's own `-H` flag.
- No "recheck declaration" action for an already-added remote server
  (e.g. after the server's own tool set changes); the stored snapshot
  is only refreshed by removing and re-adding.
