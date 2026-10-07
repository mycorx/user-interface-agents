# Cutting a release

The `release` workflow builds the installers. It does not publish them — it
leaves a **draft** GitHub Release for a human to review and publish, because a
tag is easy to push by accident and a published release is not easy to retract.

## What gets built

| Platform | Runner | Artifact |
|---|---|---|
| Windows | `windows-latest` (GitHub-hosted) | `.msi` |
| Linux | `ubuntu-latest` (GitHub-hosted) | `.deb` |
| macOS | — (local build only, for now) | none |

Windows needs a GitHub-hosted runner: an MSI is built by WiX, which only runs
on Windows. While this repository is private those minutes bill at 2×; that
stops mattering when it goes public.

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

## Cutting one

1. Bump the version in **both** manifests. They must agree:
   - `crates/uia-app/tauri.conf.json` → `version`
   - `crates/uia-app/Cargo.toml` → `[package] version`
2. Commit the bump through a PR, as usual.
3. Tag the merge commit on `main` and push the tag:

   ```bash
   git tag 0.2.0
   git push origin 0.2.0
   ```

4. Watch the run. The `version-gate` job checks the tag against both manifests
   before either build starts, so a mistyped tag fails in seconds rather than
   after a Windows build.
5. Review the draft release, write the notes, publish.

Tags are clean semver — `MAJOR.MINOR.PATCH`, optionally with a prerelease
suffix (`0.2.0-rc.1`) — with **no `v` prefix**. The tag is exactly what both
manifests say, so there is no prefix to add or strip anywhere.

A `v`-prefixed tag is rejected, and rejected *loudly*: the workflow triggers on
`v[0-9]*` as well, purely so that `v0.2.0` fails the gate with a message
telling you to use `0.2.0`. A tag filter that simply misses would produce no
run, no error and no release — silence being much worse than a red X.

To check a version bump before tagging anything:

```bash
./scripts/check-release-version.sh 0.2.0
```

## Dry runs

Run the workflow manually (**Actions → release → Run workflow**) to build every
platform without touching a release. The installers land as Actions artifacts
(`uia-linux`, `uia-windows`, and `uia-macos` once that job is enabled),
which is how you get a build to a tester without
committing to a version number.

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
