# Releasing

Releases are automated from PR labels. Contributors only label PRs; CI bumps the
version, tags every merge, and a maintainer promotes the newest tag with one
manual workflow run. The `release` workflow publishes the GitHub Release itself,
but only after the full test suite and an install-and-launch smoke test of both
installers pass.

## What gets built

| Platform | Runner | Artifact |
|---|---|---|
| Windows | `windows-latest` (GitHub-hosted) | `.msi` |
| Linux | `ubuntu-latest` (GitHub-hosted) | `.deb` |
| macOS | — (local build only, for now) | none |

Windows needs a Windows runner: an MSI is built by WiX, which only runs on
Windows. The repository is public, so standard GitHub-hosted runner minutes
cost nothing, on any platform.

macOS is not built by this workflow yet. For now it is a local build:
`scripts/build-macos-package.sh` produces an unsigned `.app` and `.dmg` (see the
header of that script), which is enough for anyone building from this public
repository. Publishing a downloadable macOS release is the part that needs an
Apple Developer ID certificate — without one, Gatekeeper quarantines a
downloaded `.app` and users cannot open it by double-clicking, which is worse
than offering nothing. So there is deliberately no macOS artifact on the
releases page until that is set up. Until then the temporary
`macos-local-build-note` job builds nothing and only leaves a notice and a
run-summary note pointing at the script.

The signed build is already written: the `macos` job at the end of
`.github/workflows/release.yaml` is commented out and lists what it needs —
the entitlement below, and the `APPLE_*` repository secrets (certificate,
signing identity, notarization credentials). It builds one universal `.app` +
`.dmg` on `macos-latest` and fails the run unless the result is signed,
carries the microphone entitlement, passes Gatekeeper and is notarized. To
switch it on, uncomment it and delete `macos-local-build-note`. Also add
`macos` to `updater-manifest`'s `needs` and pass `--require macos` to its
command, so the macOS update joins `latest.json` (see the "Before enabling"
step 3 in the workflow comments).

**Before a signed macOS release, do not forget the microphone entitlement.**
`crates/uia-app/Info.plist` already carries `NSMicrophoneUsageDescription`, which
is what makes macOS show the permission prompt at all. A build signed with a
Developer ID uses the hardened runtime, and under it the microphone is blocked
unless the app is also signed with
`com.apple.security.device.audio-input` — the prompt never appears and the app
just receives silence, exactly as it does without the usage description. Add an
`Entitlements.plist` containing that key as `true`, point
`bundle.macOS.entitlements` in `tauri.conf.json` at it, and check the result
with `codesign -d --entitlements - uia.app` before shipping. An ad-hoc local
build does not need it.

Linux ships as a `.deb` only. An AppImage was built until 2026-09-10 and was
dropped on purpose: its bundler downloads and executes unpinned content during
the release job — `linuxdeploy` from a rolling tag, `linuxdeploy-plugin-appimage`
from the literal `continuous` tag, and `linuxdeploy-plugin-gtk.sh` from `master`
— while every other step in the workflow is pinned to a commit SHA. Nothing
promised an AppImage: the README offers no download and `tauri.conf.json`
declares only `msi`.

The cost is accepted rather than unnoticed. The `.deb` depends on the
webkit2gtk-4.1 runtime, so Ubuntu 22.04 and the non-Debian distros (Fedora,
Arch, openSUSE) now have no Linux artifact at all. Bring the AppImage back —
with the bundler tools mirrored or pinned — when one of them is a real
audience.

## How a change becomes a release

1. **Label the PR.** Exactly one `semver:uai-app:<bump>` label, where `<bump>` is
   `patch`, `minor`, `major`, or `none`.
   - Any change under the app's code paths (`crates/`, `src/`, `Cargo.*`,
     `package.json`, `pnpm-lock.yaml`, the Vite and Svelte configs) needs a real
     bump, not `none`.
   - Docs, `.github/`, and `scripts/` changes ship nothing: use `none`.
2. **Fill in the PR template.** `Summary`, `Release note` (one user-facing
   sentence; `N/A` only for `none`) and `Testing`. The Release note is what ends
   up in the published release notes.
3. **CI bumps the version.** The `release-bump` bot commits the new
   `version.json` and manifests to your branch. `release-check` (a required
   status) fails until they match. If another PR merged first, the version is
   stale: the bot re-bumps on your next push or relabel. On a merge conflict in
   `version.json`, merge `main`, take `main`'s file, and run the bump again.
4. **Merging tags it.** The `tag` workflow tags the merge commit
   `uai-app-X.Y.Z` when the version is new. `none` PRs make no tag.
5. **Promote when you want to ship.** Actions → **release** → Run workflow. There
   is no version to enter: it finds the latest published release, takes the
   newest tag after it, and ships that. Tags in between are skipped, but their
   PRs are in the release notes. It runs the full CI suite at that commit
   first, then builds, smoke-tests both installers on clean machines, and
   publishes.

`version.json` is the only file anyone edits to change a version; `tauri.conf.json`,
`Cargo.toml` and `Cargo.lock` are generated from it by `scripts/release`.

Fork PRs cannot receive the bot's commit. For those, release-check prints the
fix and a maintainer pushes it:

```bash
python3 scripts/release bump --root . --base-root <checkout of main> \
  --label semver:uai-app:patch --changed-files <file listing the PR's paths>
```

Tags are `uai-app-MAJOR.MINOR.PATCH`. A future component gets its own prefix
(`uai-mobile-1.2.3`); a *suffix* such as `1.2.3-uai-app` would be a semver
prerelease and is not used.

Renovate: only a **major** cargo/npm update cuts a version (`patch`); minor and
patch updates, pins, digests, GitHub Actions and lockfile maintenance are `none`
and ride along with the next release.

## Dry runs

Run **release** with `dry_run` ticked. It tests, builds and smoke-tests, uploads
the installers as Actions artifacts (`uia-linux`, `uia-windows`, with their
`.sig` files), and creates no release.

- **From a branch** (Actions → release → *Use workflow from*: your branch): it
  builds **that branch's own commit** at the version in its `version.json`, with
  no tag needed. Use it to prove a change to the build, the dependencies or the
  workflow before it merges. The branch's manifests must agree with its
  `version.json` (the release bot keeps them in sync on a PR).
- **From `main`:** it promotes the newest *unreleased* tag, exactly as a real
  release would (with nothing to promote, `plan` fails). Use it to rehearse a
  release or hand a tester a build.

A dry run signs with a **throwaway key** generated inside the job, never the real
updater key, so it needs no environment and a branch can run it. It proves the
bundler signs and writes the `.sig` files and that the installers pass the smoke
tests; it does not prove the real key and passphrase work in CI. The first real
run proves that, and it only creates a draft, so a wrong passphrase fails before
anything is public. A real release (no `dry_run`) is refused from any branch but
`main`.

## First-time setup (one admin, once)

1. **GitHub App.** The org-owned App `automated-release` needs *Contents:
   read & write* and *Pull requests: read* (nothing else), installed on this
   repository. Store its **client ID** and a generated private key as repository
   secrets `RELEASE_APP_CLIENT_ID` and `RELEASE_APP_PRIVATE_KEY`. It is needed
   because pushes made with `GITHUB_TOKEN` do not trigger workflows, so a bump
   commit would never get its required checks. Bump commits are authored as
   `339497082+automated-release[bot]@users.noreply.github.com` (set in
   `release-bump.yaml` and listed in renovate's `gitIgnoredAuthors`).
2. **Labels.** Create `semver:uai-app:patch|minor|major|none`.
   **Tag ruleset** (Settings → Rules → Rulesets, target: tags): restrict
   creations, updates and deletions, with the bypass list limited to admins and
   the `automated-release` App. The `tag` workflow creates tags with the App's
   token, because `GITHUB_TOKEN` cannot be put on a bypass list. Add a
   *Restrict tag names* rule (must match) with:

   ```
   ^uai-[a-z][a-z0-9]*(-[a-z][a-z0-9]*)*-(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$
   ```

   That allows `uai-<component>-X.Y.Z` for any component and nothing else (no
   bare or suffixed versions). Add the App to this ruleset's bypass list only,
   not to branch rules. If `tag` fails with `HTTP 422`, the log now includes the
   rule that rejected it.
3. **Retag the first release.** The `tag` workflow refuses to run for a component
   with no `uai-app-*` tags (it would stamp the current `main` as the version in
   `version.json`). Create `uai-app-0.1.1` on the commit of the published 0.1.1
   release and move the release to it:

   ```bash
   sha="$(git rev-list -n 1 0.1.1)"
   gh api repos/{owner}/{repo}/git/refs -f ref=refs/tags/uai-app-0.1.1 -f sha="$sha"
   gh release edit 0.1.1 --tag uai-app-0.1.1 --title "uai-app 0.1.1"   # --tag renames the release's tag and keeps its assets
   git push origin :refs/tags/0.1.1 :refs/tags/0.1.0
   ```
4. **Branch protection on `main`:** require the `release-check` status, require
   branches to be up to date, require review. Do this *after* the first merge
   that contains these workflows, or nothing can merge.
5. **Environments** (Settings → Environments). A branch's workflow YAML runs
   before anyone reviews it, so secrets and the publish step are guarded by
   environments whose deployment branch is restricted to `main`:
   - `draft-release`: deployment branch `main`. Holds the secrets
     `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, used
     by the `linux` and `windows` jobs of a real run. **Delete the
     repository-level copies of those two secrets** once they are in the
     environment: a repository secret is readable from any branch's workflow.
     No required reviewers: a real run is started by hand and `publish` has its
     own gate, and reviewers here would mean one approval per build job.
   - `public-release`: deployment branch `main`, with required reviewers. Gates
     `publish`, the only job that makes a release public.
6. **Renovate** (`.github/renovate.json5`) must already carry the `semver:uai-app:*`
   labels and `gitIgnoredAuthors` (this repo's config does). Delete the old
   `semver:patch`, `semver:none` and `semver:major` labels so open Renovate PRs
   pick up the new ones.

6. **Linux smoke image.** The Linux smoke test runs in a bare `ubuntu:26.04`
   container (not the runner) so the runner's preinstalled libraries cannot hide
   a dependency the `.deb` forgot to declare. The tag is not digest-pinned in
   `release.yaml`; Renovate (`pinDigests`) opens a PR to pin it.

Recommended hardening:

- **Protect the release machinery.** CODEOWNERS or a ruleset requiring review
  does **not** stop a PR from editing its own workflow file: a `pull_request`
  run uses the PR's workflow YAML *before* anyone reviews it, so a PR can
  weaken `release-check` for itself. The real controls are: the `draft-release`
  and `public-release` environments restricted to the `main` branch (see
  first-time setup), with required reviewers on `public-release`; the signing
  key held only as an environment secret; the release App not on any bypass
  list; and write access given only to trusted people. The `Real releases run
  from main` step in `release.yaml` only stops mistakes: a branch can edit its
  own copy of it, which is why the secrets must not be reachable from a branch.
- **Keep the App minimal.** The release GitHub App must not be on any ruleset or
  branch-protection bypass list, and needs only *Contents: write* and
  *Pull requests: read*.

## Installers are unsigned

Neither published installer is code-signed, so:

- **Windows** shows a SmartScreen "unknown publisher" warning that the user
  must click through (More info → Run anyway).
- **Linux** is unaffected; the `.deb` carries no equivalent gate.

Signing Windows needs a purchased OV/EV certificate. Worth doing before any
broad public distribution; not worth blocking the first releases on.

## Updates

### How an update reaches users

The app checks for updates at `https://github.com/mycorx/user-interface-agents/releases/latest/download/latest.json`
automatically: 30 seconds after launch, then every 6 hours, unless the user has
disabled automatic checking in Settings → General → App behavior. The user can
also force a check with "Check now" in Settings, or from the tray's "Check for
updates…" menu item (which opens Settings).

When an update is available, the HUD displays a banner: "Update *X* · Install
and restart" with a "×" (Later) button. The install button is only active when
the session is Idle or Listening — it is disabled while the assistant is Speaking
or in any other state. Once clicked, the update downloads with a progress
indicator, then installs. On Windows, the MSI installer handles the exit and
restart. On Linux, `pkexec` prompts for the admin password before installation.

### The feed: `latest.json`

The `.github/workflows/release.yaml` workflow builds `latest.json` from the
signed installer artifacts attached to the draft release. The `updater-manifest`
job runs `python3 scripts/release updater-manifest` to generate this file, then
attaches it to the draft. The `publish` job waits for `updater-manifest` to
complete before it runs — this ensures the feed is ready when the release is
published.

### Publish: when every install gets access

`publish` is the moment a release reaches every installed copy of the app.
Once a release is published, `/latest/` points to it and every app checking for
updates will see it.

To withdraw a bad release, delete it from the GitHub Releases page or move it
back to draft status. The `/latest/` endpoint then falls back to the previous
published release. Installs that have already downloaded and installed the
withdrawn release remain on that version. Fix forward with a new version.

### The signing key

The updater verifies every downloaded installer using a public key baked into
the app at build time. That public key is in `crates/uia-app/tauri.conf.json`
under `plugins.updater.pubkey` (currently `F3E43EB66DE69A6C`).

**Generate the keypair once, before the first release with the updater:**

```bash
pnpm exec tauri signer generate -w ~/.tauri/uia.key
```

The command prompts for a passphrase and saves the keypair to `~/.tauri/uia.key`.
Create two secrets **in the `draft-release` environment** (not as repository
secrets; see first-time setup): `TAURI_SIGNING_PRIVATE_KEY` (the full contents of
`~/.tauri/uia.key`) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (the passphrase).
The release workflow reads these secrets automatically in a real run.

**Store both the private key and passphrase securely.** The keypair is not
rotatable in any ordinary sense: every installed client trusts exactly the key
compiled into its binary. Losing the private key strands every install on
whatever version it has. GitHub Actions secrets are write-only and cannot be
read back, so the copy in `TAURI_SIGNING_PRIVATE_KEY` is not a backup. Escrow
a second copy in AWS Secrets Manager (`ap-southeast-2`) at the same time you
generate it:

```bash
# The file:// form makes the CLI read the key off disk, so the private key
# never appears in a terminal, a shell history, or a log.
aws secretsmanager create-secret \
  --name uia/updater-signing-key \
  --description "Tauri updater private key for the uia desktop client. Not rotatable: losing it strands every installed client." \
  --secret-string file://~/.tauri/uia.key \
  --region ap-southeast-2
```

...and the passphrase as `uia/updater-signing-key-password`. Default
AWS-managed encryption is fine and keeps idle cost at about US$0.40/secret/month;
no rotation schedule, for the reason above. It goes in the non-production
AWS account only because that is the one that exists — move it to the
production account once that is stood up.

The release workflow uses the secrets `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` to sign installers in the `linux` and
`windows` jobs of a real run, through the `draft-release` environment. A dry run
signs with a throwaway key instead (see *Dry runs*), so the real key is never
reachable from a branch.

### Local builds

The local packaging scripts (`scripts/build-macos-package.sh` and
`scripts/build-windows-package.ps1`) turn off updater artifacts (feed
generation and installer signing) unless `TAURI_SIGNING_PRIVATE_KEY` is set in
the environment. With `createUpdaterArtifacts` on, the bundler fails outright
when it has no private key to sign with, so without this a local build would
need the release key; only builds with the private key set (the release
workflow) produce signed installers.

## Linux builds refuse to run under WSL

The `.deb` exits immediately with an explanation when launched
from a WSL shell. uia needs a tray icon, a system-wide hotkey, and a
microphone; WSL provides none of them dependably, and without the gate the app
fails much later inside GTK or cpal with an error that names none of this.

WSLg can render GTK windows, so if the check is ever wrong, `UIA_ALLOW_WSL=1`
overrides it. The detection logic lives in `crates/uia-app/src/wsl.rs`.
