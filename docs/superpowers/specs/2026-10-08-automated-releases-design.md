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
    "legacy_tag": "{version}",
    "label": "uai-app",
    "synced": ["crates/uia-app/tauri.conf.json",
               "crates/uia-app/Cargo.toml",
               "Cargo.lock"],
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
- **Legacy bare tags:** `legacy_tag` lets `uai-app` recognise the existing
  `0.1.0` / `0.1.1` tags as its own history. They count for baseline lookup
  and for "does this version already have a tag"; new tags always use the
  prefix. No published tag is moved or renamed.
- `code_paths` / `ignore_paths` define "code changed" (confirmed).

### 3. `scripts/release/` — one tested script, used everywhere

Python, no third-party deps, fixture tests in the style of
`scripts/tests/release-version/`. Subcommands:

- `check-labels` — validates a PR's labels against its changed files.
- `bump` — computes `next = bump(version.json on the base branch, label)` and
  writes it to `version.json`, then runs `sync`. Idempotent. Also the command
  a human runs locally.
- `sync` / `sync --check` — writes (or verifies) `version.json`'s versions in
  each component's `synced` files: `tauri.conf.json`, `[package] version`, the
  `uia-app` entry of `Cargo.lock`.
- `tag-for <component>` — the tag name `version.json` implies.
- `plan <component>` — the release plan (§6).

The PR check, the bump bot, the tagger and the release workflow all call this
script, so they cannot disagree. It supersedes `check-release-version.sh`
(kept as a thin wrapper until `RELEASE.md` is updated).

### 4. PR workflows

**`release-bump`** (`pull_request`: `opened, synchronize, reopened, labeled,
unlabeled`). If the PR carries a valid label and `version.json` is not at the
expected value, the bot commits the correction to the PR branch (`bump`). The
bot's commit retriggers CI, so the checks re-run on the final head. Relabelling
re-computes. Fork PRs cannot receive a bot push; for them the check fails with
the exact fix command (`./scripts/release bump`) for the author or a
maintainer to run.

**`release-check`** — required status, every PR. Fails unless:
1. every `semver:*` label names a registered component, and there is at most
   one per component. A PR touching two components needs one label for each;
2. a component's label is not `none` (and is present) when the PR changes any
   of its `code_paths`. A PR touching no component's code needs a single
   `semver:<any>:none`;
3. `version.json` equals `bump(version.json on base, label)` (always the base's
   current version, so a stale PR is red until re-bumped);
4. `sync --check` passes (no drift between `version.json` and the manifests).

`none` PRs: version unchanged, check 3 requires exactly that.

### 5. Workflow: `tag` (on push to `main`)

For each component: read `version.json`; if the implied tag does not exist,
create it on `${{ github.sha }}` through the API. Nothing is built or
released. A `none` merge changes nothing, so no tag. Idempotent, so re-runs are
harmless. Permission: `contents: write`. Tags created with the default token
do not trigger other workflows, which is intended.

### 6. Workflow: `release` (`workflow_dispatch`, no version input)

Input: `component` (default `uai-app`) and `dry_run` (default false) — neither
is a version.

`plan` job (`scripts/release plan`):
1. **Baseline** = the latest *published* (non-draft, non-prerelease) release of
   the component, found by tag prefix (plus `legacy_tag` for `uai-app`).
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
5. Creates the draft release for the target (reusing an existing draft for it),
   with GitHub's generated notes using `previous_tag_name = baseline`, so the
   notes cover **every PR merged since the last published release**, including
   the skipped intermediate tags.

Then, unchanged from today but checked out at the **target tag**:
`linux` (.deb) and `windows` (.msi) build in parallel and attach to the
draft. New:

- `tests` — the full `ci.yaml` suite at the target tag. `ci.yaml` gains a
  `workflow_call` trigger with a `ref` input and checks that ref out (a
  dispatch run's own SHA is `main`'s head, not the tag).
- `smoke-windows` / `smoke-linux` — on a clean runner, install the built
  `.msi` / `.deb` and check it. The app has no CLI and, on Windows, is built
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
| `version.json` stale because `main` moved | `release-check` red; the bot re-bumps on the next push to the PR (or the author runs `./scripts/release bump`). |
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

- Add `version.json` = `{"uai-app": "0.1.1"}`: `0.1.1` is the published
  release (tag `0.1.1`, `fe3ef77`). The first automated tag will be
  `uai-app-0.1.2` or higher. The published `0.1.0` / `0.1.1` tags and releases
  are left untouched and recognised through `legacy_tag`; the prefix
  convention applies from here on. (Retagging them as `uai-app-0.1.1` would
  drop the `legacy_tag` special case but means rewriting a published release;
  not worth it.)
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

## Trade-offs accepted

- Every labeled merge changes `version.json`, so concurrent PRs conflict on it
  and must be updated before merging. Fine for a few contributors; the cost
  grows with open-PR count.
- Fork PRs cannot be bumped automatically.
- Many unpublished tags may accumulate between releases; they are cheap and
  carry no release object.

## Out of scope

Auto-promotion on a schedule, prerelease/RC flows, yanking a published
release, signed installers, the auto-updater, `uai-mobile` build jobs (only the
registry and label scheme are made ready for it).

## Open questions

All resolved:
- globs confirmed;
- tags are `uai-app-X.Y.Z`, with `legacy_tag` covering the existing bare tags;
- the published release is `0.1.1`, so `version.json` starts there;
- bot identity is a GitHub App, created manually by the org admin;
- the smoke test uses installer metadata plus a `--smoke` early-exit flag.

Setup task for the admin, outside the code: create the App, install it on this
repo, and add its id and private key as repo secrets (`RELEASE_APP_ID`,
`RELEASE_APP_PRIVATE_KEY`). The implementation plan lists the exact steps.
