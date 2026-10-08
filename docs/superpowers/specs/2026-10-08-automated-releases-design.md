# Automated releases — design

Status: draft for review · Date: 2026-10-08 · Supersedes the earlier "release PR" draft

## Goal

Replace the manual release ritual (edit two manifests, commit, tag, push the
tag, publish the draft) with:

- **Every labeled merge to `main` gets a version and a tag**, automatically.
- **Promotion is manual and batched.** A maintainer runs one workflow with no
  inputs; it promotes the newest unreleased tag, with release notes covering
  everything since the last published release.
- Contributors do exactly one thing: put a `semver:<component>:<bump>` label
  on their PR.

First component is `uai-app` (the Tauri desktop app). `uai-mobile` must be
addable later without redesign.

## Decisions already made

| Topic | Decision |
|---|---|
| Versioning | **Per merge.** Each merged PR with a non-`none` label produces one new version and one tag on its merge commit. |
| Releasing | **Manual, batched.** Tags accumulate; running the `release` workflow promotes the newest one. Intermediate tags stay unpublished. |
| Bump input | Mandatory PR label `semver:<component>:{patch,minor,major,none}`. |
| Who bumps | CI (a bot) writes the bump into the PR; a required check enforces it. Contributors never edit versions. |
| Single source of truth | `version.json`, keyed by component. |
| Release input | None. The workflow derives everything from the latest published release. |
| Publish | Draft → published automatically, only after the full test suite and the installer smoke tests pass. |

There is no release PR.

## Components

### 1. `version.json`

```json
{ "uai-app": "0.1.0" }
```

The only file edited to change a version. Adding a component is adding a key.

### 2. `.github/release-components.json` — the registry

```json
{
  "uai-app": {
    "tag": "uai-app-{version}",
    "label": "uai-app",
    "synced": [
      { "kind": "tauri-conf", "path": "crates/uia-app/tauri.conf.json" },
      { "kind": "cargo-toml", "path": "crates/uia-app/Cargo.toml" },
      { "kind": "cargo-lock", "path": "Cargo.lock" }
    ],
    "code_paths": ["crates/**", "src/**", "Cargo.toml", "Cargo.lock",
                   "package.json", "pnpm-lock.yaml", "vite.config.ts",
                   "svelte.config.js"],
    "ignore_paths": ["docs/**", "**/*.md", ".github/**", "scripts/**"]
  }
}
```

- **Tags are component-prefixed: `uai-app-1.2.3`, `uai-mobile-0.4.0`.** The
  prefix, not a suffix, because `1.2.3-uai-app` is valid semver for a
  *prerelease* of 1.2.3: it sorts below 1.2.3, and our own prerelease rule
  (`tag.includes('-')`) and GitHub's tooling would misread it. A prefix keeps
  the version a clean semver after the prefix is stripped, groups tags
  alphabetically, and `git tag -l 'uai-app-*'` selects exactly one component's
  tags. This is also what release-please and changesets do.
- `code_paths` / `ignore_paths` define "code changed" (confirmed).

### 3. `scripts/release/` — one tested script, used everywhere

Python, no third-party deps, fixture tests in the style of
`scripts/tests/gen-notice/`. Invoked as `python3 scripts/release <command>`.
Commands: `check-pr`, `bump`, `sync [--check]`, `current`, `tag`, `plan`,
`notes`.

- `check-pr` — validates a PR's labels, version and description against its
  changed files.
- `bump` — computes `next = bump(version.json on the base branch, label)` and
  writes it to `version.json`, then runs `sync`. Idempotent. Also the command
  a human runs locally.
- `sync` / `sync --check` — writes (or verifies) `version.json`'s versions in
  each component's `synced` files: `tauri.conf.json`, `[package] version`, the
  `uia-app` entry of `Cargo.lock`.
- `current` — the version `version.json` holds.
- `tag <component>` — the tag name `version.json` implies.
- `plan <component>` — the release plan (§6).

The PR check, the bump bot, the tagger and the release workflow all call this
script, so they cannot disagree. It replaces the removed
`check-release-version.sh`.

### 4. PR workflows

**`release-bump`** (`pull_request`: `opened, synchronize, reopened, labeled,
unlabeled`). If the PR carries a valid label and `version.json` is not at the
expected value, the bot commits the correction to the PR branch (`bump`). The
bot's commit retriggers CI, so the checks re-run on the final head. Relabelling
re-computes. Fork PRs cannot receive a bot push; for them the check fails with
the exact fix command (`python3 scripts/release bump`) for the author or a
maintainer to run.

Both `release-bump` and `release-check` read the PR from
`--event "$GITHUB_EVENT_PATH"`, and read the registry from the base checkout
(never the PR's), so a PR cannot edit its own rules.

**`release-check`** — required status, every PR. Triggers on the same events
as `release-bump` plus `edited`, so fixing the description re-runs it. Fails
unless:
1. every `semver:*` label names a registered component, and there is at most
   one per component. A PR touching two components needs one label for each;
2. a component's label is not `none` (and is present) when the PR changes any
   of its `code_paths`. A PR touching no component's code needs a single
   `semver:<any>:none`;
3. `version.json` equals `bump(version.json on base, label)` (always the base's
   current version, so a stale PR is red until re-bumped);
4. `sync --check` passes (no drift between `version.json` and the manifests);
5. the PR description passes `check-pr-body` (see "PR template" below).

`none` PRs: version unchanged, check 3 requires exactly that.

### PR template and description check

`.github/pull_request_template.md` has three sections:

- **Summary** — what changed and why.
- **Release note** — one user-facing sentence, or `N/A` on `none`-labelled PRs.
- **Testing** — how it was verified.

GitHub only pre-fills a template, so the requirement is enforced by
`scripts/release check-pr-body` (inside `release-check`). After stripping HTML
comments (so untouched placeholders count as empty), it fails when a heading
is missing, a section is empty or still the placeholder, or **Release note** is
`N/A` on a PR whose label is not `none`. The Release note sections are also
what the release notes are built from (§6).

### Renovate

The repo uses Renovate with automerge (`.github/renovate.json5`: squash,
`platformAutomerge: false`, `prConcurrentLimit: 5`, weekly groups for Rust
crates and GitHub Actions, daily `lockFileMaintenance`). Its PRs hit the same
rules as any PR.

**Policy: only a *major* dependency update cuts a version, and only a `patch`.
Everything else Renovate does is `none`.** Minor and patch updates, pins,
digests, GitHub Actions and lockfile maintenance never create a version or a
tag; they ride along in the next release, because a release builds the
promoted tag's commit and that includes everything merged before it. Because
the automerged PRs leave `version.json` alone, they need no bump commit, never
conflict on it, and cost no extra runs.

Changes in `renovate.json5`:

- **Label names:** `semver:*` becomes `semver:uai-app:*`.
- **One label per PR.** `addLabels` accumulates across every matching rule, so
  the rules must not overlap. Today the minor/patch rule also matches the
  `github-actions` manager. Make them disjoint:
  - major updates (any manager) → `semver:uai-app:patch` (not automerged, as
    today);
  - every other update type, including `github-actions` → `semver:uai-app:none`.
- **`gitIgnoredAuthors`** lists the release App's commit author. Renovate
  treats a branch with foreign commits as externally modified and stops
  rebasing it; this stops the bump commit counting. It only matters for major
  PRs (the only ones that get a bump commit): when Renovate rebases it drops
  the bump commit, `release-bump` re-adds it on the push.
- Grouping npm minor/patch like the crates and Actions is no longer needed for
  versioning and is left as a separate tidy-up.

**Security fixes wait like any other update** (decided). This is a local
desktop application with manual installs and no auto-updater, so cutting a
version sooner does not get a fix to users sooner; the promotion cadence is the
bottleneck, not the tagging. Vulnerability-alert PRs are labelled
`semver:uai-app:none` like other non-major updates. If an urgent fix needs to
ship at once, a maintainer labels that PR `patch` by hand and runs `release`.

In `release-check`:

- **Renovate PRs are exempt from rule 2** (label vs `code_paths`). They are
  labelled by package rule, not by a human who can judge. Exactly-one-label,
  rule 3 (version math) and `sync --check` still apply. This is what lets
  `lockFileMaintenance` and minor/patch updates (which rewrite `Cargo.lock` /
  `pnpm-lock.yaml`, both in `code_paths`) stay `none`, per the policy above.
- **Renovate PRs are exempt from `check-pr-body`;** their title stands in for
  the Release note, listed under "Dependencies".

**Setup order:** change `renovate.json5` **before** `release-check` becomes a
required status, or automerge stalls on every open Renovate PR.

### 5. Workflow: `tag` (on push to `main`)

For each component: read `version.json`; if the implied tag does not exist,
create it on `${{ github.sha }}` through the API. Nothing is built or
released. A `none` merge changes nothing, so no tag. Idempotent, so re-runs are
harmless. Permission: `contents: write`. Tags created with the default token
do not trigger other workflows, which is intended.

### 6. Workflow: `release` (`workflow_dispatch`, no version input)

Input: `component` (default `uai-app`) and `dry_run` (default false) — neither
is a version.

The `plan` job's first step fails unless `github.ref == refs/heads/main`. A
`verify-target` job then runs `sync --check --expect-version` at the target SHA,
so the tagged commit's manifests are proven to match the tag before anything is
built.

`plan` job (`scripts/release plan`):
1. **Baseline** = the latest *published* (non-draft, non-prerelease) release of
   the component, found by tag prefix.
   GitHub's single "latest release" is never used, because with several
   components it would answer for whichever shipped last. If none exists, the baseline is "nothing" and every tag
   counts.
2. **Candidates** = all tags matching the component's `tag` template (prefix
   stripped before comparing versions) with a
   semver **greater than** the baseline. None → fail with "nothing to
   release".
3. **Target** = the highest candidate. It must be a descendant of the
   baseline's commit and reachable from `main`; otherwise fail loudly.
4. Outputs: target tag, its SHA, baseline tag, and the list of candidate tags
   (shown in the run summary so the maintainer sees exactly what ships).
5. Creates the draft release for the target (reusing an existing draft for it).
   The notes are built by `scripts/release notes` from the **Release note**
   section of every PR merged between the baseline and the target, grouped by
   bump label (`major` first, as breaking changes) with a PR link per line.
   Working from PRs rather than tags means the skipped intermediate tags are
   covered automatically. Renovate PRs, which have no such section, are listed
   under "Dependencies" using their titles.

Then, unchanged from today but checked out at the **target tag**:
`linux` (.deb) and `windows` (.msi) build in parallel and attach to the
draft. New:

- `tests` — the full `ci.yaml` suite at the target tag. `ci.yaml` gains a
  `workflow_call` trigger with a `ref` input and checks that ref out (a
  dispatch run's own SHA is `main`'s head, not the tag).
- `smoke-windows` / `smoke-linux` — on a clean runner, install the build job's
  uploaded artifact (`.msi` / `.deb`) and check it. The Linux job runs in a bare
  `ubuntu:24.04` container. The app has no CLI and, on Windows, is built
  with `windows_subsystem = "windows"` (no console output), so the assertions
  use installer metadata and exit codes, not stdout:
  1. install silently (`msiexec /i /qn` / `dpkg -i`) and require success;
  2. the installed version equals the target version (MSI `ProductVersion` /
     the exe's file version; `dpkg -s` `Version`);
  3. Linux: `ldd` on the installed binary shows no missing libraries (this is
     the check that would have caught the webkit2gtk runtime dependency);
  4. **launch check** — run the installed binary with a new early-exit flag
     `--smoke` and require exit code 0 (Linux under `xvfb-run`);
  5. uninstall cleanly.

  `--smoke` is the **only app change** this work needs: a few lines at the top
  of `main()` (like the existing WSL guard), before config and Tauri start, that
  does nothing but `exit(0)`. It proves the binary loads and links on a clean
  machine without opening a window. It prints nothing and touches no config or
  keyring. macOS stays a local build until the Developer ID certificate exists.
- `publish` — only when `tests` and both smoke jobs are green; flips the draft
  to published and marks it latest.

`dry_run: true` runs the same builds and tests against the target but uploads
Actions artifacts and creates no release. It replaces today's dispatch dry
run.

Concurrency: one `release` group, `cancel-in-progress: false`.

The old tag-push trigger is removed.

## Failure behaviour

| Situation | Result |
|---|---|
| PR with no / two semver labels | `release-check` red; cannot merge. |
| Code change labelled `none` | Same. |
| `version.json` stale because `main` moved | `release-check` red; the bot re-bumps on the next push to the PR (or the author runs `python3 scripts/release bump`). |
| Two PRs bumped the same version | Second one conflicts on `version.json` when updated from `main`; resolution is `git merge main`, take `main`'s file, rerun `bump`. |
| `release` with no newer tags | Fails: "nothing to release". |
| Build, tests or smoke fail | Draft stays unpublished. Re-run `release`; it re-derives the same target and reuses the draft. |
| Target not on `main` / not a descendant of baseline | `plan` fails before anything is created. |

## Setup this needs (admin)

1. **GitHub App** for `release-bump` (org-owned, `contents:write` +
   `pull-requests:write`, installed on this repo), with its id and private key
   as repo secrets. The default `GITHUB_TOKEN` cannot be used: its pushes do
   not retrigger workflows, so the bump commit would never get its required
   checks.
2. Branch protection on `main`: require `release-check`, require branches up to
   date, require review.
3. If tag rulesets exist, allow the workflow to create tags.
4. Create the labels `semver:uai-app:{patch,minor,major,none}`.

## Migration

- **One-time, manual, before the workflows are enabled** (nobody uses the app
  yet, so rewriting the first release is acceptable):
  1. delete the published release and tag `0.1.1` (and the `0.1.0` tag);
  2. create tag `uai-app-0.1.1` on `fe3ef77` and publish a release for it.
  Order matters: the `tag` workflow tags any `version.json` version that has no
  tag, so if `version.json` says `0.1.1` while `uai-app-0.1.1` does not exist,
  it would tag the current `main` HEAD as `0.1.1`.
- Add `version.json` = `{"uai-app": "0.1.1"}`. The first automated tag will be
  `uai-app-0.1.2` or higher. There is no legacy-tag handling in the code.
- Remove the tag-push trigger from `release.yaml`; update `docs/RELEASE.md`
  ("Cutting one" becomes "run the `release` workflow").
- `ci.yaml`: add `workflow_call` with a `ref` input.

## Testing

- Script unit tests with fixtures: label validation (zero/two/unknown labels,
  `none` vs code paths), bump arithmetic, `sync` round-trip and `--check` drift
  on all three files, and `plan` — baseline selection ignoring drafts and
  prereleases, candidates strictly newer, highest-wins, descendant check,
  no-candidates failure.
- Workflow logic lives in the script; workflows stay thin.
- `release` dry run before the first real release.

## Build vs reuse

No existing action implements this flow, so the version logic is our own
script. Evaluated: **release-please** (conventional-commit driven, centred on
a release PR; would replace the label scheme and per-merge versions) and
**release-drafter** (label-driven with `tag-prefix` and a `resolved_version`
output, but it drafts releases itself, has no manifest bumping and no
per-component path logic; its notes would be built from PR titles, where we want the PR template's Release note sections). Reused instead where an
action is the commodity part: `actions/create-github-app-token` (bot token),
`tauri-apps/tauri-action` and `actions/github-script` (already in use).

## Trade-offs accepted

- Every labeled merge changes `version.json`, so concurrent PRs conflict on it
  and must be updated before merging. Fine for a few contributors; the cost
  grows with open-PR count.
- Fork PRs cannot be bumped automatically.
- Many unpublished tags may accumulate between releases; they are cheap and
  carry no release object.

## Out of scope

Enforcing the template on issues, a conventional-commit title check,
auto-promotion on a schedule, prerelease/RC flows, yanking a published
release, signed installers, the auto-updater, `uai-mobile` build jobs (only the
registry and label scheme are made ready for it).

## Open questions

All resolved:
- globs confirmed;
- tags are `uai-app-X.Y.Z` only, no legacy-tag support; the first release is retagged by hand (see Migration);
- the published release is `0.1.1`, so `version.json` starts there;
- bot identity is a GitHub App, created manually by the org admin;
- the smoke test uses installer metadata plus a `--smoke` early-exit flag.

Setup task for the admin, outside the code: create the App, install it on this
repo, and add its id and private key as repo secrets (`RELEASE_APP_ID`,
`RELEASE_APP_PRIVATE_KEY`). The implementation plan lists the exact steps.
