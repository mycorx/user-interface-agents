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
switch it on, uncomment it and delete `macos-local-build-note`.

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

Run **release** with `dry_run` ticked. A dry run needs an *unreleased* tag (one
newer than the latest published release; with nothing to promote, `plan` fails)
and must be dispatched **from `main`** (the workflow refuses any other branch).
It plans, tests, builds and smoke-tests the
same target, uploads the installers as Actions artifacts (`uia-linux`,
`uia-windows`), and creates no release. Use it to hand a tester a build and to
rehearse a release.

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
5. **Renovate** (`.github/renovate.json5`) must already carry the `semver:uai-app:*`
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
  weaken `release-check` for itself. The real controls are: a `release` GitHub
  environment restricted to the `main` branch, with required reviewers, on the
  `create-release` and `publish` jobs; the release App not on any bypass list;
  and write access given only to trusted people.
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

## There is no auto-updater yet

Deliberate, as of 2026-09-06: shipping the updater means generating a signing
keypair, escrowing it, and baking a public key into every build. At zero users
that ceremony buys nothing, so updates are manual — download the new installer
from the Releases page and run it.

**This has a deadline.** An app built without the updater has no public key
compiled in and never checks for updates, so it can never be *migrated* onto an
update channel later — everyone already running a build has to reinstall once,
by hand, after being told to. That cost is nil today and grows with every
install. Add the updater before the repository goes public (~2026-10-05) or
before there is a real install base, whichever comes first.

When that happens, the keypair is not rotatable in any ordinary sense: every
installed client trusts exactly the key baked into its binary, so losing the
private key strands the entire fleet on whatever version it has. GitHub Actions
secrets are write-only and cannot be read back, so the copy in
`TAURI_SIGNING_PRIVATE_KEY` is not a backup. Escrow a second copy in AWS
Secrets Manager (`ap-southeast-2`) at the same time you generate it:

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

## Linux builds refuse to run under WSL

The `.deb` exits immediately with an explanation when launched
from a WSL shell. uia needs a tray icon, a system-wide hotkey, and a
microphone; WSL provides none of them dependably, and without the gate the app
fails much later inside GTK or cpal with an error that names none of this.

WSLg can render GTK windows, so if the check is ever wrong, `UIA_ALLOW_WSL=1`
overrides it. The detection logic lives in `crates/uia-app/src/wsl.rs`.
