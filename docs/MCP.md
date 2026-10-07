# MCP servers

UIA gives the assistant tools by connecting to MCP servers. They're added
from the app's **Settings → MCP** tab, never by hand-editing config — the
old `uia.toml` `[[mcp.servers]]` list (any command, any URL, no install
step, no record of what was approved) is gone. What's approved now lives in
`uia-mcp.json`, next to `uia.toml`, written only by the Settings UI.

Exactly two transports are supported.

## Local: `.mcpb` bundle

A `.mcpb` is a zip containing a `manifest.json` that declares a command to
launch. UIA extracts it, validates it, and — only if it passes — records it
as an installed local server and runs it as a subprocess over stdio. Extraction
restores each file's permission bits from the archive and makes the manifest's
launch command executable, so a bundle's binary is runnable after install. For
a bundle installed before this fix, the launch command is made executable at
launch if it has no execute bit.

**Desktop only.** iOS's sandbox forbids spawning subprocesses and Android
heavily restricts it, so a `.mcpb` local server is unavailable on mobile builds.
(`McpServerConfig::is_desktop_only()` in `crates/uia-mcp/src/client.rs`.)

### Manifest fields UIA acts on

`user_config`, `server`, and `name`/`version` are acted on. Everything else
in the MCPB schema (`description`, `author`, `tools`, `dxt_version`, ...) is
parsed but ignored — an unknown key must never make an otherwise-valid bundle
uninstallable, and none of those fields affect what gets executed.

```json
{
  "name": "my-tool",
  "version": "1.0.0",
  "server": {
    "type": "binary",
    "entry_point": "bin/my-tool",
    "mcp_config": {
      "command": "${__dirname}/bin/my-tool",
      "args": ["--stdio"],
      "env": { "SOME_VAR": "value" },
      "platform_overrides": {
        "win32": { "command": "${__dirname}/bin/my-tool.exe" }
      }
    }
  }
}
```

- `server.type` must be `"binary"` — `"node"` and `"python"` are parsed but
  rejected at validation, not installable. This does not mean a Python or
  Node server is out of reach; see [what the rule actually
  enforces](#what-the-rule-actually-enforces-self-contained-not-compiled).
- `${__dirname}` in `command`/`args` is substituted with the bundle's
  extracted directory.
- `platform_overrides` is keyed by Node's platform names (`win32`,
  `darwin`, `linux`), not Rust's, and merges field-by-field over the base
  config — an override naming only a Windows `.exe` still keeps the base
  `args`.

### Validation (`crates/uia-mcp/src/bundle/validate.rs`)

Four checks, deliberately overlapping — no single one is trusted alone:

1. **`server.type` must be `"binary"`.** The bundle's own claim.
2. **The resolved command must stay inside the extracted bundle directory**
   (a textual `.`/`..` fold, not `canonicalize` — the file need not exist
   yet and resolution must not depend on the host filesystem's symlinks).
3. **The command's extension must not be one that means "an interpreter
   runs this file"** (`.py`, `.js`, `.sh`, `.ps1`, `.bat`, ... — see
   `SCRIPT_EXTENSIONS`). Evidence against the `"binary"` claim.
4. **The first two bytes of the resolved file must not be `#!`.** Catches
   an extensionless script the extension check can't see.

Together, these stop a manifest from pointing at an interpreter already on
the machine and calling itself a binary local server, or from launching anything
outside the bundle it shipped in.

### Settings: `user_config`

A manifest's `user_config` declares settings the user fills in once. UIA shows
them under **Settings → MCP → (server) → Configuration** and substitutes
`${user_config.<key>}` in `args` and `env` at launch, after validation — so a
setting can never change which executable runs. `${user_config.*}` in `command`
is not supported: it is refused at validation (`UserConfigInCommand`), because a
setting must never choose what runs.

Value order: the saved value, then the manifest `default`, then an error if
the field is `required` (the server shows as Failed with the reason). Substitution
is a single pass — a value that itself contains `${...}` is never expanded again.
Plain values are stored in `uia-mcp.json`; `sensitive` ones only in the OS keyring
(macOS Keychain, Windows Credential Manager, Secret Service on Linux) under
`mcp-config.<server>.<key>`, and are deleted when the server is removed. Sensitive
values are never returned to the UI; Settings only learns whether one is set.
Supported types: `string`, `number`, `boolean`, `directory`, `file`; numbers
must be finite and within `min`/`max` if declared; a `multiple` field uses its
first value only. Saving is all-or-nothing — validation and required checks run
first, then keyring writes are rolled back if a later write or the registry write
fails. Changes apply on the next restart.

A sensitive value placed in `args` is visible to anyone who can list processes,
so prefer `env` for secrets.

**Path variables.** In the manifest's `default`, `args` and `env` values, UIA
expands `${HOME}`, `${DESKTOP}`, `${DOCUMENTS}`, `${DOWNLOADS}`, `${/}` and
`${pathSeparator}` (the last two are the OS path separator), alongside
`${__dirname}`. They are never expanded in `command` (it must stay inside the
bundle), in `env` keys, or in a value the user typed. A variable UIA does not
know, or that the OS cannot name on this machine, is left in the text unchanged
and does not fail the server. On Windows `${HOME}` contains
backslashes, so authors who want native separators should use `${/}`.

**Forgiving fields.** Only the attributes of each `user_config` field are read
leniently: a wrongly typed attribute (a numeric `title`, say) falls back to its
default instead of failing the whole manifest, and an entry that is not an
object is skipped. An unparseable `sensitive` is treated as sensitive, so a
secret never ends up in plain text. The rest of the manifest stays strict.

**Form behavior.** Defaults are shown in the form but stored only when you
change them: a value equal to the default is not saved, and saving it clears an
earlier stored value. A `boolean` setting, or a string setting whose declared
default is exactly `true` or `false`, is shown as a toggle (it still stores
that text); a stored value alone does not make a toggle, a string setting with
no default stays a text box, and sensitive and `multiple` fields never are.
Number settings are typed as text and refused at save if not a number;
`min`/`max` are enforced when saving.

### What the rule actually enforces: self-contained, not compiled

The property being protected is **self-containment** — everything that
executes came out of the zip the user reviewed and approved — and not
"native code is safer than a script". It isn't: a native binary can do
anything a Python script can, so binary-only was never a sandbox.

Read the four checks against that goal and they split in two. Checks 1, 3
and 4 block the *script*; check 2 blocks the *interpreter*. Check 2 is the
one carrying the security property, because it is what rules out
`npx -y some-server@latest` — a command that fetches its real code from the
network at every launch, *after* the user approved it, and that a third
party can change at any time. In that arrangement the user approves a
manifest, not code. That is the pattern this validation exists to refuse.

**So a Python or Node server is not forbidden — it just has to bring its
own interpreter.** A bundle that vendors one and launches it is
self-contained, and installs today:

```json
{ "server": { "type": "binary",
    "mcp_config": { "command": "${__dirname}/py/bin/python3",
                    "args": ["${__dirname}/server.py"] } } }
```

The command is a real executable inside the bundle: it has no script
extension, no `#!`, and does not escape the bundle directory. Both the
interpreter and the script shipped in the reviewed artifact, so
containment holds. Usually simpler still is to collapse the server into a
single executable first — `bun build --compile`, `deno compile`, or
PyInstaller — which needs no vendored runtime directory at all.

Two caveats, both deliberate:

- **`args` are substituted but not validated** (`resolve_launch` in
  `bundle/manifest.rs`; `validate_bundle` checks `.command` only). That is
  not a hole: a vendored interpreter is already arbitrary native code, so
  passing it a script grants nothing extra. It does mean the checks
  constrain *what is launched*, not *everything it is handed*.
- This supersedes the rationale in
  `docs/superpowers/specs/2026-08-29-mcp-plugin-bundles-design.md`, which
  described `"node"`/`"python"` bundles as rejected because they "imply a
  bundled interpreter running bundled source". A bundled interpreter
  running bundled source is in fact the supported shape; what is rejected
  is reaching for an interpreter that is *not* in the bundle.

There is deliberately **no "allow unsafe bundles" setting**. Relaxing
checks 1, 3 and 4 alone would not even work — a bundle holding a bare
`server.py` still could not launch, because `python3` is not inside it and
check 2 rejects it. Making scripts genuinely runnable means relaxing check
2 as well, which is the whole model. Anything that truly cannot be made
self-contained belongs on the remote HTTP transport below.

## Remote: HTTP(S) server

A remote server must answer the MCP `initialize` handshake and declare its
tools live, at the moment it's added — there is no way to register a server
"blind" the way a `.mcpb`'s manifest can be inspected offline. That live
declaration is what gets recorded and shown in Settings as approved.

- URL must be `http://` or `https://` with a non-empty host and no embedded
  whitespace (`settings::is_valid_http_url`).
- An optional single auth header (name + value) is sent on every request.
  The value is never logged and never round-tripped back out in debug
  output — only the header *name* is shown.
- A newly added remote server starts **disabled**; the user enables it
  explicitly.
- A server's own `enabled` flag is the whole trust decision; there is no
  registry-wide switch above it.

This is also the only MCP transport available on mobile builds, since
stdio subprocesses aren't.

## Status

Settings shows a **Status** column per server, and a warning line beneath any
row that is not contributing. Two different events feed it, and they answer
two different questions:

- **Did it connect?** `session::build_executor` records the outcome of the one
  connection attempt made when the running session started.
- **Did it contribute any tools?** `McpRouter::list_tools` reports each
  server's outcome — listed, failed, timed out — through a `ListingObserver`,
  and that runs on *every* `Session::connect`.

Both write into `session::McpHealth`; `list_mcp_server_health` reads it back.

| Status | Meaning |
| --- | --- |
| Connected | It connected, and contributed at least one tool to the session as it now stands. |
| No tools | It connected, but the session has none of its tools: it listed none, its listing failed, or it did not answer within the deadline — the reason is shown in full on its own line. |
| Failed | It was enabled but did not connect, or was dropped before the attempt — the reason is shown in full on its own line. |
| Pending restart | Enabled, but the running session never attempted it: added or enabled after startup, or no session has started. |
| Off | Not enabled, so never attempted. |

It is still **not a probe.** Nothing re-checks a server because Settings is
open, and MCP registry changes stay restart-gated throughout — the registry is
read once, at startup, so a server whose name is absent from the health map is
reported as pending restart, never as healthy.

It is no longer a **startup-only snapshot**, and that is a deliberate change
from what this document promised before. Listing re-runs on every connect —
each reconnect, the max-session-duration rotation, and every persona switch —
and each run rewrites that server's status. A server that connected cleanly at
launch and went unresponsive an hour later now reads "No tools" from the next
connect onwards, instead of holding an hour-old `Connected`. The refresh is
driven by the session, not by the panel: Settings reads the map when it opens,
so a panel left open still shows what was true when it last read.

"Failed" covers two different origins deliberately: a local server dropped by
`mcp_targets` (files missing, unreadable manifest, no longer passes bundle
validation) and a server that reached a connection attempt and was refused.
Both were enabled by the user and both are silently absent from the session,
which is the thing the column exists to make visible.

## One namespace

Local and remote server names share one namespace: a remote server cannot
be added under a name an installed local server already uses, or vice versa. A
server's name is the routing prefix every one of its tools is reachable
under, so a collision would silently misroute calls.

## What the assistant actually sees

The `uia-mcp.json` record (name, declared tools, enabled flags) is what
Settings shows a human as "approved." It is never what's handed to the
model. The live tool list always comes fresh from `McpRouter::list_tools()`
at session start — so a stale-looking entry in Settings can mislead a
person reading it, but never the assistant's own tool declaration.

### One slow server does not cost you the others

Servers are asked **concurrently**, each under its own deadline, and a server
that fails or does not answer in time is skipped with its tools left out — the
session still gets every other server's. Two things follow from that:

- A server that is **down** answers promptly with an error, and always did.
  A server that is **wedged** — connected, accepting the request, never
  replying — used to hold the session open indefinitely, because this call
  sits on the critical path in `Session::connect`. It is now bounded.
- Because a persona switch reconnects the session, an unbounded listing there
  would have stalled a switch the user is actively waiting on. `Session`
  keeps its own outer deadline over the whole call as a backstop for any
  executor, not just this one.

The cost of being skipped is the assistant not having those tools for that
connect — but it is no longer silent about it. Each server's outcome is
reported through a `ListingObserver`, so the **Status** column above shows
"No tools" with the reason, refreshed on the next connect. Skips still go to
stderr as well, for a run with no Settings panel open to read them.

`uia-mcp` deliberately does not know what those outcomes are *for*. The router
takes a trait; the app supplies the handle that writes into `McpHealth`. That
is what keeps this crate free of app state, and it is why the observer is
optional — a router built without one lists tools exactly as it did before.
