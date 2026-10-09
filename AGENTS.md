# Agent instructions

## Pull requests

- Fill in `.github/pull_request_template.md` and keep its headings exactly:
  `## Summary`, `## Release note`, `## Testing`. `release-check` fails if any is
  empty. Write `N/A` under Release note only when the label is `none`.
- Add exactly one `semver:uai-app:<patch|minor|major|none>` label. `none` is only
  valid when no app code changed (see `.github/release-components.json`).
- Do not edit `version.json` or the version in `tauri.conf.json`, `Cargo.toml` or
  `Cargo.lock` by hand. The release bot syncs them; to do it locally run
  `python3 scripts/release bump --help`.
- Release process and setup: `docs/RELEASE.md`.
