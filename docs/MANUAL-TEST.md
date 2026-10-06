# SP1 manual acceptance (Windows)

Everything here needs a real machine with a microphone, speakers, a GUI, and a
Windows/macOS host that can compile `tauri`. **None of it can be run in the
development sandbox** — WSL2 has no audio device, and `--features desktop`
fails to compile at `libdbus-sys` before webkit2gtk is even reached (S13).
That is why this file exists as a human checklist rather than a test.

What is *not* here, because it is measured automatically instead:
time-to-first-audio and tool-calling reliability. Those live in
`crates/uia-app/tests/provider_ab.rs` and
`docs/superpowers/findings/2026-08-16-provider-ab.md` — driven by synthesised
speech so the numbers are reproducible, which a human at a microphone cannot be.

## Prerequisites (Windows)

`pnpm run tauri dev` is not a pure Node command — the Tauri CLI shells out to
`cargo` to compile the Rust backend in `crates/uia-app/`. None of this is
optional; skipping any step fails the first `cargo build`, not `pnpm install`.

1. Install Rust via [rustup](https://rustup.rs) (rustup-init.exe), accepting
   the default **MSVC** toolchain (`x86_64-pc-windows-msvc`), not the GNU one.
2. Install the **Desktop development with C++** workload from the
   [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
   installer — this provides the MSVC linker `cargo` needs on Windows. Rust
   alone will not link without it.
3. Confirm **WebView2** is present (Settings → Apps → search "WebView2
   Runtime"). It ships with Windows 10/11 by default; if missing, install the
   [Evergreen Runtime](https://developer.microsoft.com/microsoft-edge/webview2/).
4. Install [Node.js](https://nodejs.org) (LTS) if not already present.
5. Verify the toolchain before touching this repo:
   ```powershell
   rustc --version
   cargo --version
   node --version
   ```

This is a one-time setup per machine. Once done, `pnpm run tauri dev` triggers
a `cargo build` on first run (slow — several minutes) and incremental builds
after that.

## Setup

`pnpm install` needs the full repo, not just `uia.toml` copied out on its
own — `package.json` and the `crates/uia-app` Rust sources it depends on
only exist inside the clone. Clone the repo itself first:

```powershell
cd C:\Users\<you>\Downloads    # or wherever you keep checkouts
git clone https://github.com/mycorx/user-interface-agents.git
cd user-interface-agents
```

Then, from inside that clone:

```powershell
copy uia.example.toml uia.toml   # then edit
$env:OPENAI_API_KEY = "sk-..."         # or set openai.key_file in uia.toml
pnpm install
pnpm run tauri dev
```

`pnpm run tauri dev` starts the Vite dev server on `http://localhost:1420`
itself (via `beforeDevCommand` in `tauri.conf.json`) before launching the
Rust/Tauri shell — one command, one terminal. If you see it hang on `Warn
Waiting for your frontend dev server to start on http://localhost:1420/...`
for more than a few seconds, something else is already bound to port 1420;
free the port and retry.

If you already have a `uia.toml` or API key file from an earlier attempt,
move them into this cloned repo's root rather than running `pnpm install`
against a folder that only has config files in it — a bare folder like that
has no `package.json` and `pnpm install` will fail with `ENOENT`.

For the Nova pass, either click **Nova (fallback)** in the HUD's engine
selector and restart the assistant (S18 — the choice persists to
`uia-engine.json`, next to `uia.toml`, and overrides
`engine.default` on the next launch), or set `engine.default = "nova"` in
`uia.toml` directly. Either way, make sure AWS credentials resolve
(`aws sts get-caller-identity`). `nova.region` must be one of `us-east-1`,
`us-west-2`, `ap-northeast-1` — `ap-southeast-2` is rejected at load time,
on purpose.

## Checklist

Run the whole list once on OpenAI, then again on Nova — either via
`engine.default` in `uia.toml` or the HUD's selector (see above). Record
a failure with what you actually observed, not just a cross.

**Both passes complete as of 2026-08-21.** The OpenAI pass (below) found and
fixed two real bugs — the hotkey toggle and an unreadable terminal-error
message, both detailed inline. The Nova pass (region `ap-northeast-1`,
credentials via AWS SSO profile `sso-nonprod`) reproduced the same walk with
no new defects: the hotkey toggle, audio quality/AEC, barge-in, and MCP tool
calling all behaved identically to the OpenAI pass. The only expected
difference between engines is latency (see "Known gaps to expect").

### Shell (S13's surface, unverified until now)

- [x] `Ctrl+Shift+Space` shows the overlay; pressing it again hides it
      — **originally FAILED**: the overlay only showed while the keys were
      held, hiding again on release. Root cause:
      `tauri_plugin_global_shortcut`'s `on_shortcut` callback fires once for
      key-down and once for key-up, and the handler forwarded `Toggle` on
      both, so a single press toggled itself back off. Fixed in
      `activation_desktop.rs` by only forwarding on `ShortcutState::Pressed`.
      Confirmed working (proper toggle, not press-and-hold) on both engine
      passes.
- [x] The overlay is frameless, transparent, centred, always-on-top, and absent
      from the taskbar — pass. Note: a setting was since added to let the user
      drag the window, which was not part of the original spec but does not
      contradict it.
- [x] Tray icon present; Show / Hide / Quit all work — the tray entry is
      present and functional, but the icon itself renders as a ghost/blank
      (transparent) glyph rather than a visible icon. Likely the same
      placeholder-icon gap noted in "Known gaps to expect" below.
- [x] The engine selector (S18, fixed S21) shows two buttons, OpenAI
      preselected and Nova visually marked as the fallback; clicking Nova
      shows a "restart to apply" notice with a **Restart Now** button (S21)
      that actually relaunches the app, rather than requiring a manual
      kill-and-relaunch; after relaunch (by that button or by hand) the
      selector highlights **the engine the session actually started with**,
      confirmed correct even when it differs from what was highlighted
      before restarting. **Confirmed on real Windows hardware 2026-08-21** —
      the selector now shows the right engine after both restart paths.
- [x] Ask "which engine are you running on?" (S21) and the assistant names it
      correctly (OpenAI Realtime / AWS Nova Sonic) — a spoken cross-check
      against the selector's highlight, independent of the HUD. This is what
      caught S21's bug in the first place: the selector showed "OpenAI"
      selected after a restart into Nova, while the voice answering was
      Nova's the whole time.

### Session

- [x] Speaking produces a spoken reply
- [x] Status text tracks `Idle → Connecting → Listening → Speaking`
      (`Connecting` is brief but must be visible — it is broadcast, not
      coalesced, precisely so it cannot be missed)
- [x] The level meter moves while you speak and settles when you stop
- [ ] The meter drops to flat the moment the session disconnects — the idle
      timeout is the easiest way to see it — rather than holding the last
      level it was given
- [x] Talking over the assistant stops playback within ~50 ms (barge-in)
- [x] Asking for the MCP tool's data calls the tool and the reply is spoken

### Personas

Most of this frame is covered by `cargo test` and was walked through in a live
window. What is listed here is what automation genuinely cannot reach: the
native file dialog, and whether a switch *sounds* like a switch.

- [ ] **Add photo…** opens the OS picker, and a chosen JPEG or PNG appears in
      the editor header, on the list row, and in the HUD
- [ ] An **animated GIF still animates** in all three of those places — this
      is the one that a naive "resize everything" would silently break, so it
      is worth watching rather than assuming
- [ ] A large photo (a phone camera original) is accepted, and the stored
      `uia-agent-avatar-<id>.<ext>` is far smaller than the file you picked
- [ ] A file renamed to `.png` that is not a PNG is refused, with the reason
      shown in the frame rather than nothing happening
- [ ] With two personas enabled and the switch toggle on, asking the assistant
      to switch produces a spoken confirmation, a pause, and then replies in
      the new persona's manner
- [ ] Speaking *during* that pause is lost, not queued — expected, and the
      reason the gap was measured (`crates/uia-app/tests/rotation_gap.rs`).
      What must no longer be silent about it: for the whole pause the status
      reads **`… — not listening`**, the dot beside it goes hollow, and the
      waveform sits flat rather than freezing mid-shape. The loss is by
      design; being told nothing about it was not. This one wants watching
      rather than assuming — the window is a median 842 ms, so the check is
      whether it is legible at that length, not merely whether it renders
- [ ] After restarting, the assistant is back to the persona Settings marks
      active, not the one it was switched to

### Failure behaviour

- [x] Unplugging the active output device does not crash the app — logs show
      `uia-audio: input stream error: The stream configuration is no
      longer valid and must be rebuilt.`
- [x] Killing the network mid-session shows `Connecting` and recovers when the
      network returns (backoff: 500 ms base, doubling, 30 s cap)
- [x] An invalid API key shows a distinct error state and does **NOT**
      retry-loop — the session stops rather than spinning. **Originally the
      error text was not user-legible**: a `connect()` failure (which is
      exactly what an invalid key produces) never reached `error_tx`, so only
      the bare `uia: session ended: Terminal` ever showed, with no reason.
      Fixed in `session/mod.rs`'s `connect()` to broadcast `e.to_string()`
      before the terminal transition, matching how a mid-session
      `EngineEvent::Error` already behaved; covered by a new regression test
      (`a_terminal_connect_failure_is_broadcast_not_just_the_bare_outcome`).
      Not independently re-exercised with a bad key on the Nova pass, but the
      code path is shared across both engines.
- [ ] Switching engine via the UI selector (S18, restart-to-switch) reaches
      the new engine on the next launch, from any state — this is
      restart-to-switch, not live switching, so `can_switch_engine`'s
      `Idle`-gate is not exercised through the UI (see "Known gaps to
      expect"): `Session::switch_engine()` itself is still only reachable
      through the API, and that half remains untested through the UI.

### Audio quality (S15/S17, and the one thing only a room can test)

**Before this section, check Settings → System → Sound.** On Windows the app
now opens the devices Windows has configured for the **Communications** role,
not the Console/default role, because that is the role its built-in echo
cancellation is keyed to. If those two roles point at different physical
devices, the assistant will use a different mic/speaker than every other app on
the machine — expected behaviour, but confusing if you are not looking for it.

- [ ] At startup, stderr carries both
      `uia-audio: wasapi capture opened under AudioCategory_Communications`
      and the matching `wasapi render opened` line. This is the plumbing check:
      it proves the OS echo canceller was actually engaged. If you instead see
      `could not open the Windows communications audio devices ... falling back
      ... WITHOUT echo cancellation`, the checks below will fail by design.
- [x] With speakers (not headphones) at a normal volume, the assistant does not
      hear itself: it must not interrupt or answer its own voice. No build flag
      needed on Windows — echo cancellation is on by default there now. This
      was previously blocked (the WASAPI integration was not yet implemented);
      confirmed fixed on both the OpenAI and Nova passes.
- [x] Your own voice still gets through clearly while the assistant is quiet
      — confirmed on both passes.
- [ ] Barge-in still cuts playback promptly (the device buffer is 20 ms, so
      audio already handed to Windows keeps playing that long past a `clear()`)
      — not separately confirmed this pass.
- [ ] Setting `aec_enabled = false` under `[audio]` in `uia.toml` (S20)
      genuinely disables cancellation: stderr no longer shows the
      `AudioCategory_Communications` lines above, and with speakers at a
      normal volume the assistant now *does* hear and interrupt/answer its
      own voice — confirming the toggle isn't a no-op. Either symptom
      (own-voice audible in a loopback test, or the barge-in RMS check firing
      on playback) is sufficient evidence cancellation is genuinely absent.

## Known gaps to expect

- **`crates/uia-app/icons/` is a placeholder icon set**, not a real logo —
  generated only so `tauri-build` has an `icon.ico` to embed as the Windows
  exe resource (its absence fails the build outright with `icons/icon.ico`
  `not found`). Swap it for the real product icon via `pnpm run tauri icon
  <path-to-1024px-source.png>` once one exists; don't treat the current
  solid-color square as final art.

- **`--features aec` still does not build on Windows, and no longer needs to.**
  `webrtc-audio-processing-sys` 2.1.0 (the only version on crates.io) fails
  under MSVC with `error C7555: use of designated initializers requires at
  least '/std:c++20'` — the vendored WebRTC C++ source uses C++20 syntax the
  crate's bundled meson config never requests for MSVC, only ever having been
  built/tested in S15's WSL2/Linux sandbox. Fixing it means forking the C++
  vendor tree. **Superseded on Windows by S17**, which uses the OS's own
  AEC/AGC/NS via WASAPI's Communications category instead — no C++ toolchain,
  on by default, nothing to pass. `--features aec` remains the path for
  Linux/macOS and is untouched; do not pass it on Windows.

- **The engine selector is restart-to-switch, not live** (landed S18).
  Clicking Nova/OpenAI in the HUD persists the choice to
  `uia-engine.json` and shows a "restart to apply" notice; the running
  session is untouched until the app restarts. PLAN.md's `Idle`-gated live
  switching (`can_switch_engine`, S4/`Session::switch_engine()`) remains the
  target but stays reachable only through the API — this control does not
  call it. **Entirely unverified against real Tauri IPC**: `main.rs` still
  cannot be compiled in this sandbox (S13's `libdbus-sys` blocker,
  re-confirmed unchanged by S18), so a human must confirm the selector
  actually shows, persists, and takes effect on restart before treating this
  gap as closed.
- **Nova is the fallback and is measurably slower** (~380 ms more to first
  audio, most of it a Sydney↔Tokyo round trip). Slower is expected; broken is
  not.

## Where config and produced files live

Two modes, decided by one question: is there a `uia.toml` in the current
directory or any ancestor?

**Development** -- a checkout. Everything stays beside that `uia.toml`,
exactly as it always has. Running from the repo root puts the sidecars in the
repo root.

**Packaged** -- no `uia.toml` anywhere above. Everything goes to one per-user
directory named `uia-client`:

| OS | Path |
|---|---|
| Windows | `%LOCALAPPDATA%\uia-client` |
| macOS | `~/Library/Application Support/uia-client` |
| Linux | `$XDG_CONFIG_HOME/uia-client`, default `~/.config/uia-client` |

Local, not roaming, on purpose: `uia-audio.json` stores audio *device names*,
and an unmatched device is a deliberate hard error in `uia-audio`. A roaming
profile could carry a device name to a machine without that hardware and
refuse to start audio.

`UIA_CONFIG_DIR` overrides both -- point it anywhere to run against a scratch
directory, or to keep a portable install self-contained.

Secrets are not files: they live in the OS keyring under service `uia`.

The sidecars that end up there, all beside `uia.toml` (or in `uia-client`):

| File | Written by |
|---|---|
| `uia-engine.json` | Settings → Providers, the engine choice |
| `uia-agent.json` | Settings → General, assistant name and app behaviour |
| `uia-audio.json` | Settings → Audio, device names and AEC |
| `uia-mcp.json` | Settings → MCP, approved servers |
| `uia-personas.json` | Settings → General → Personas… |
| `uia-agent-avatar-<id>.<ext>` | one per persona that has an avatar |
| `uia-conversation.jsonl` | the transcript, when memory is on |

`uia-personas.json` is created on first use, holding one `Default` persona.
An install predating personas also has a `uia-agent-avatar.<ext>` with no id
in the name: that image is adopted onto the default persona on first run, and
the old file is left where it is. The old `uia-agent.md` free-text persona is
**not** migrated — it has no honest split into the structured fields — so it
stays on disk, orphaned and unread.

- [ ] From the repo root, the app reads the repo `uia.toml` and no
      `uia-client` directory is created.
- [ ] From a directory with no `uia.toml` above it, `uia-client` is created
      in the location above, and settings changes persist there.
- [ ] With `UIA_CONFIG_DIR` set, both of the above defer to it.

## Remote MCP server with OAuth

Requires a **bundled build** on Windows or macOS. The custom `uia://` scheme
this depends on is registered by the installer, not by `pnpm run tauri dev`,
and none of it is reachable in WSL2 at all (no display, and `--features
desktop` cannot compile here — see the top of this file).

This is Settings → MCP → "Remote servers" → "Add a server", using its
"Sign in with a browser" authentication mode rather than "Static header".

### First sign-in

1. Build and install the bundle so the `uia://` scheme is registered with the
   OS.
2. Open Settings → MCP. Under "Remote servers" → "Add a server", enter a
   **Name** and the server's **URL**.
3. Under **Authentication**, choose **Sign in with a browser**. The static
   header fields disappear — the backend refuses an OAuth server that also
   carries a static `Authorization` header, so the form never offers the
   combination.
4. Enter the **OAuth client ID** (the public app client id registered for uia with the server's
   identity provider; not a secret under PKCE).
5. Click **Connect**. The system's *default browser* opens to the identity
   provider's hosted login page (not an in-app window — `@tauri-apps/plugin-opener`'s
   `openUrl()` hands it to the OS). Sign in there.
6. Back in the app, the form now shows **"Waiting for you to finish signing
   in…"** with an **"I've signed in — check again"** button, a **"Reopen the
   sign-in page"** link, and **Cancel**. The Connect button is gone for the
   duration — clicking Connect again mid-flow is not possible from this
   screen.
7. After finishing the browser sign-in, click **"I've signed in — check
   again"**. There is no push notification for "the browser round trip
   finished" (a deliberately deferred gap — nothing in the backend fires a
   Tauri event for it yet), so this button is a manual retry, not a status
   readout: it re-runs the same connection attempt and the form updates once
   it succeeds. Clicking it repeatedly BEFORE finishing the browser sign-in
   is safe: the backend now resumes the same pending sign-in (same CSRF
   state, same authorize URL) instead of starting a second one, so repeated
   clicks neither open a second background wait nor abandon the first one's
   progress — confirm this by clicking "check again" two or three times in a
   row before returning to the browser; nothing should misbehave, and the
   eventual real sign-in should still complete normally.
8. On success the form shows the server's declared name, version, and tool
   list, with **Confirm & add** / **Cancel** buttons.
9. Click **Confirm & add**. The server is saved **disabled**, per the
   existing rule (nothing added ever starts enabled).
10. Enable it (the checkbox next to its name in the list) and confirm the
    declared tools appear when you click **Show tools**.
11. **The actual acceptance check:** leave the session running for over an
    hour, then invoke one of the server's tools through the assistant. It must work with no
    prompt and no pasted token. This is the thing that fails today without
    this phase's work.
12. Restart the app entirely. It must still be signed in — nothing needs
    re-adding, and no browser prompt reappears — because the refresh token
    came from the OS keychain, not memory.

### Session expired (refresh token past its lifetime)

Cognito's refresh token defaults to 30 days; this cannot be waited out in a
normal test pass, so exercise it by revoking the session from the Cognito
side (or wait out a shorter TTL if the test pool is configured with one).

1. With a server already added and enabled from the flow above, invalidate
   its refresh token (revoke the user's session in the Cognito console, or
   let it expire).
2. In Settings → MCP → "Add a server", re-enter that **same name and URL**,
   choose **Sign in with a browser** again, and click **Connect**.
3. The form should show **"Session expired — sign in again"** rather than
   the first-sign-in wording — the app recognizes the name as one that is
   already added and enabled, so a fresh `NeedsAuthorization` here reads as
   an expired session rather than a new one.
4. Sign in again in the browser, then click **"I've signed in — check
   again"**.
5. Once it reports success, the form shows **"Signed in."** with a **Done**
   button instead of **Confirm & add** — the server is already in the list,
   so there is nothing new to save; signing in again only refreshed the
   credential in the OS keychain that the existing entry already points at.
   Click **Done** to close the form.
6. Confirm the previously-added entry keeps working (invoke one of the server's
   tools through the assistant) without ever appearing twice in the remote-server
   list.

### Cancelling mid-flow

1. Start a "Sign in with a browser" Connect as above, but do not finish
   signing in in the browser.
2. Click **Cancel** on the "waiting for you to finish signing in…" panel.
   The form resets to blank immediately.
3. There is no backend command to cancel the pending session it started —
   this only abandons the frontend's own wait. Confirm nothing bad happens:
   no server is added, and the app keeps working normally. (The abandoned
   background wait times out on its own after five minutes; nothing to
   verify here beyond "the app doesn't hang or crash".)

### Keychain unavailable

1. With the OS keychain/credential manager locked, unavailable, or denied
   (platform-specific — e.g. a locked GNOME Keyring on Linux, or denying the
   macOS Keychain access prompt), attempt to add or sign in to an OAuth
   remote server.
2. Confirm the failure surfaces as the same plain, non-alarming inline error
   text used everywhere else in this tab (the red line under the Connect/
   Confirm buttons) — **not** a modal dialog, an unhandled exception, or any
   wording implying data loss. It should read like "not signed in", not like
   a crash.
