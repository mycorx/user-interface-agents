# In-app updates, via the Tauri updater

## Problem

uia has no updater. A new version means downloading the installer from the
Releases page and running it, and nobody is told one exists. `docs/RELEASE.md`
already sets the deadline. A build without the updater has no public key
compiled in and never checks for updates. So it can never be moved onto an
update channel later, and everyone running it has to reinstall once, by hand,
after being told to. That cost is nil until the first published release and
grows with every install. No release tag exists yet, so `0.1.0` can ship with
the updater and no install will ever need migrating.

## Decisions (agreed 2026-10-07)

| Question | Decision |
|---|---|
| What happens when a new version exists | The app **notifies; the user confirms**. Background checks, a HUD banner with "Install and restart", and a manual "Check for updates" in Settings and the tray. An install never interrupts a live voice session. |
| Which releases are offered | **Stable only**: published, non-prerelease releases. Prereleases (`0.2.0-rc.1`) are installed by hand by testers. |
| Can automatic checks be turned off | **Yes**: a toggle that is **on by default**. Off still leaves the manual check. |
| Where the logic lives | **Rust**. Rust schedules the checks, applies the session-state rule and emits events. The webview only renders and calls app commands. |

## Platform support

- `tauri-plugin-updater` supports `.msi` (Windows) and, since plugin 2.10.0
  (2026-02-03), `.deb` (Linux). Those are exactly the two artifacts the
  release workflow ships.
- A `.deb` update is installed through a `pkexec` admin-password prompt.
- On Windows the installer exits the app itself when the install step runs.
- macOS (`.app.tar.gz` + `.sig`) joins automatically once the commented
  `macos` release job is enabled with a Developer ID certificate.

## App side

### `crates/uia-app/src/updater.rs`

**Pure part (no `tauri`, unit tested):**

- `UpdateStatus` — `Idle | Checking | UpToDate { current } |
  Available { version, notes } | Downloading { percent } | Failed { message }`.
  It serialises to the `uia://update` event payload.
- `next_check_delay(first: bool) -> Duration`: 30 s after startup, so a check
  does not compete with the session connecting, then every 6 h.
- `may_install(state: uia_core::session::State) -> bool`: true only for `Idle`
  and `Listening`. False for `Connecting`, `Thinking`, `ToolRunning`,
  `Speaking` and `Interrupting`.

**Desktop part (`desktop` feature, wraps `tauri-plugin-updater`'s Rust API):**

- A background task runs the schedule while automatic checks are on. It
  re-reads the setting before each check, so the toggle takes effect without
  a restart.
- Every status change is emitted as `UPDATE_EVENT = "uia://update"` and kept
  in managed state.
- Commands (app commands are not permission-gated, so the webview gets no
  updater permission):
  - `get_update_status() -> UpdateStatus`, so a card that mounts late still
    sees an available update.
  - `check_for_update() -> UpdateStatus`, a manual check that works whether
    or not automatic checks are on.
  - `install_update()`:
    - refuses with an error unless `may_install` holds for the current
      session state
    - downloads, emitting `Downloading { percent }`
    - installs, then restarts (`app.restart()`; on Windows the installer has
      already exited the app)

### Configuration

`tauri.conf.json` gains:

- `bundle.createUpdaterArtifacts: true`
- `plugins.updater.pubkey`: the public half of the signing key
- `plugins.updater.endpoints`:
  `["https://github.com/mycorx/user-interface-agents/releases/latest/download/latest.json"]`
- `plugins.updater.windows.installMode: "passive"`

GitHub's `/releases/latest/` never resolves to a draft or a prerelease. That
*is* the stable-only channel, with no extra logic. A release is offered to
every install the moment a human publishes it, and not before.

### Setting

- `AgentSettings.auto_update_check: Option<bool>`. `None` means on.
- Settings → General → App behavior → "Check for updates automatically",
  hint "Applies immediately."
- Below it:
  - the running version
  - a "Check now" button
  - the last result: "Up to date (0.1.0)", "0.2.0 available", or the failure
    message

### HUD

- On `Available`, a banner: "Update 0.2.0 available · Install and restart",
  with a "Later" button that hides it until the next launch.
- While downloading, a progress bar.
- While `may_install` is false, the button is disabled with a tooltip saying
  it will be available when the assistant is quiet.

### Tray

- A "Check for updates…" item runs `check_for_update` and shows the HUD.

## Release side

### Signing key

The key cannot be rotated: every installed client trusts exactly the key
compiled into it, so losing the private key strands every install on its
current version.

- The maintainer generates it locally (`pnpm exec tauri signer generate -w
  ~/.tauri/uia.key`, with a passphrase).
- The maintainer stores it as the GitHub secrets `TAURI_SIGNING_PRIVATE_KEY`
  and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and escrows both in AWS Secrets
  Manager (`ap-southeast-2`) as `docs/RELEASE.md` describes.
- Only the public key enters the repository.

### Workflow

- **Build jobs** (`linux`, `windows`, and the commented `macos`):
  - receive the two signing secrets as environment variables
  - set tauri-action's `includeUpdaterJson: false`
  - attach only the installer and its `.sig`
  - on dry runs (`workflow_dispatch`), sign as well and upload the `.sig`
    with the artifact, so the key is exercised before any tag
- **New `updater-manifest` job:**
  - tag pushes only
  - `needs: [create-release, linux, windows]`
  - reads the draft's assets and builds `latest.json`:
    - `version`, `notes` and `pub_date`
    - per platform: `url` and `signature` (the `.sig` file's contents) for
      `windows-x86_64-msi`, `linux-x86_64-deb`, and `darwin-aarch64` /
      `darwin-x86_64` (one universal `.app.tar.gz`) once macOS assets exist
  - uploads it in one write
- **Why a separate job:**
  - tauri-action's own `latest.json` handling is read–modify–write on the
    release.
  - The `linux` and `windows` jobs run in parallel, so the second writer
    could drop the first one's platform — the same race `create-release`
    exists to avoid.
- **The manifest job fails the run if:**
  - an installer has no `.sig`, because one unsigned entry makes every
    client reject the update
  - the version is a prerelease or `v`-prefixed
- **Assembly logic:**
  - lives in `scripts/updater_manifest.py`, so the GitHub step is only glue
  - has fixture tests in `scripts/tests/updater-manifest/`, run in
    `version-gate` like the existing release-version tests
- CI (`ci.yaml`) never runs `tauri build`, so it needs neither the key nor
  any override.

### Documentation

- `docs/RELEASE.md`: replace "There is no auto-updater yet" with:
  - how an update reaches users
  - the key steps, and that the key cannot be rotated
  - the fact that publishing a draft is the moment it goes live to every
    install
- `docs/MANUAL-TEST.md`: add an "Updates" section (below).

## Failure handling

- **A check fails** (offline, GitHub down, malformed `latest.json`):
  - the status becomes `Failed`
  - the HUD shows nothing, because background failures are silent
  - Settings shows the message
  - the next scheduled check retries
- **Signature mismatch.** The plugin rejects the download and the status
  shows "Update failed signature check". Nothing is installed, and that
  version is not offered again in this run.
- **Download fails partway.** Nothing is installed, and the banner returns to
  "Install and restart".
- **The user cancels the Linux `pkexec` prompt.** This is a `Failed` status,
  not a crash; the app keeps running.
- **The updater never affects the voice session, the tray or settings.** A
  failure anywhere in it is reported through `UpdateStatus` and nowhere else.

## Testing

- **Rust unit tests:**
  - `next_check_delay`
  - `may_install` for every `State` variant
  - the `UpdateStatus` → payload mapping
  - the `auto_update_check` default
  - `uia://update` is covered by `check-ipc-event-names.sh`
- **Python fixture tests for `updater_manifest.py`:**
  - Windows and Linux
  - Windows, Linux and macOS
  - a missing `.sig` fails
  - a prerelease or `v`-prefixed version is rejected
- **Manual, once per OS before the first public release** (needs two
  published stable versions, because `/latest/` skips prereleases):
  1. Install `0.1.0`, then publish `0.1.1`.
  2. The banner appears within about 30 s.
  3. "Install and restart" relaunches as `0.1.1`.
  4. With the toggle off, no banner appears, but "Check now" still finds the
     update.
  5. While the assistant is speaking, the install button is disabled.

## Out of scope

- A beta or prerelease channel.
- Silent or background installs.
- Delta updates.
- Rotating the key. It is not possible; see above.
