# In-app updates, via the Tauri updater

## Problem

uia has no updater. A new version means downloading the installer from the
Releases page and running it, and nobody is told one exists. `docs/RELEASE.md`
already sets the deadline. A build without the updater has no public key
compiled in and never checks for updates. So it can never be moved onto an
update channel later, and everyone running it has to reinstall once, by hand,
after being told to.

The releases published so far (`uai-app-0.1.0`, `uai-app-0.1.1`) shipped
without the updater, but nobody has installed them (confirmed 2026-10-09). So
they are ignored. The first release that carries the updater is treated as
the baseline, and no install needs migrating.

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
every install the moment the `release` workflow's `publish` job flips it from
draft to published (`make_latest: true`), and not before. Every gate (tests,
installer smoke tests, the update feed) has passed by then.

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

The `release` workflow is run by hand from `main`:

```
plan -> verify-target -> tests -> create-release (draft)
     -> linux / windows (parallel)
     -> smoke-linux / smoke-windows
     -> publish
```

- `plan` picks the newest unreleased `uai-app-X.Y.Z` tag and outputs
  `target_version`. That is the bare `X.Y.Z` from `version.json`, which is
  also what the app reports as its own version.
- A `dry_run` input builds and tests without creating a release.

The changes:

- **Build jobs** (`linux`, `windows`, and the commented `macos`):
  - receive the two signing secrets as environment variables, on dry runs
    too, so the key is exercised before any real release
  - set tauri-action's `includeUpdaterJson: false`
  - attach only the installer and its `.sig`
  - also add the `.sig` to the uploaded Actions artifact (`uia-linux`,
    `uia-windows`)
- **New `updater-manifest` job:**
  - skipped on dry runs
  - `needs: [plan, create-release, linux, windows]`
  - runs in parallel with the smoke tests
  - reads the draft's assets and builds `latest.json`:
    - `version` (`plan.outputs.target_version`), `notes` (the draft's body)
      and `pub_date`
    - per platform: `url` and `signature` (the `.sig` file's contents) for
      `windows-x86_64-msi` and `linux-x86_64-deb`, plus `darwin-aarch64` /
      `darwin-x86_64` (one universal `.app.tar.gz`) once macOS assets exist
  - uploads it to the draft in one write
- **`publish` gains `updater-manifest` in its `needs`.** A release therefore
  never goes live without a valid feed. The commented macOS instructions
  (step 3) also add `macos` to `updater-manifest.needs`.
- **Why a separate job:**
  - tauri-action's own `latest.json` handling is read–modify–write on the
    release.
  - `linux` and `windows` run in parallel, so the second writer could drop
    the first one's platform — the same race `create-release` exists to
    avoid.
- **The manifest step fails the run** (keeping the release a draft) if:
  - an expected installer is missing
  - an installer has no `.sig`, because one unsigned entry makes every
    client reject the update
  - the version is not plain `X.Y.Z` semver
- **Assembly logic:**
  - is a new `updater-manifest` subcommand of the existing `scripts/release`
    CLI (beside `plan`, `notes` and `sync`), so the workflow step is only
    glue
  - its unit tests go in `scripts/tests/release/`, which `ci.yaml` already
    runs
- CI never runs `tauri build`, so it needs neither the key nor any override.

### Documentation

- `docs/RELEASE.md`: replace "There is no auto-updater yet" with:
  - how an update reaches users
  - the key steps, and that the key cannot be rotated
  - that the `publish` job is the moment a release goes live to every
    install, so a bad release is withdrawn by deleting or re-drafting it
    rather than by waiting
  - the macOS enablement steps: `macos` also joins `updater-manifest.needs`
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
- **Python unit tests for `scripts/release updater-manifest`** (in
  `scripts/tests/release/`):
  - Windows and Linux
  - Windows, Linux and macOS
  - a missing installer fails
  - a missing `.sig` fails
  - a version that is not plain `X.Y.Z` is rejected
- **Manual, once per OS, with the first two releases that carry the updater**
  (call them N and N+1). This needs two published releases, because
  `/latest/` only ever points at the newest published one:
  1. Install N from its release, then release N+1.
  2. The banner appears within about 30 s.
  3. "Install and restart" relaunches as N+1.
  4. With the toggle off, no banner appears, but "Check now" still finds the
     update.
  5. While the assistant is speaking, the install button is disabled.

## Out of scope

- A beta or prerelease channel.
- Silent or background installs.
- Delta updates.
- Rotating the key. It is not possible; see above.
