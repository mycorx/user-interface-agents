# Contributing to UIA

Thank you for your interest in contributing to MycorX.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). Reports go
to info@mycorx.com.au.

## Reporting a security vulnerability

Do **not** open a public issue for a security vulnerability. Report it privately
through GitHub Security Advisories — see [SECURITY.md](SECURITY.md) for the
process and what's in scope.

## Pull requests

You are welcome to submit pull requests that:

- Fix bugs
- Improve documentation
- Add enhancements
- Suggest architectural improvements

## Contributor License Agreement

Before your first contribution can be merged, you need to accept the
[MycorX Individual Contributor License Agreement](CLA.md), once. Add this
trailer to a commit in your first pull request:

    git commit --trailer "MycorX-CLA-Accepted: v1.0"

The `cla` CI job checks for it and explains what to do if it is missing. After
that pull request merges you are added to
[CONTRIBUTOR_LEDGER.md](CONTRIBUTOR_LEDGER.md) automatically, and later pull
requests need no trailer.

**You keep the copyright in what you write.** The CLA grants MycorX a broad
license to use, sublicense, and distribute your contribution — it is not an
assignment of ownership. MycorX needs the sublicense right because UIA is offered
under the PolyForm licenses and, separately, under commercial licenses by written
agreement; without it, your contribution could not be included in both.

If you are contributing as part of your job, your employer may own the copyright.
Have someone authorized to bind your employer email info@mycorx.com.au before
you submit, so a corporate agreement can be arranged.

## Forking

UIA is licensed under PolyForm Internal Use 1.0.0 or PolyForm Noncommercial
1.0.0 ([LICENSE.md](LICENSE.md)). The Internal Use license does not permit
distribution at all, and a public GitHub fork is distribution — so LICENSE.md
grants one narrow exception: you may fork this repository for the sole purpose of
opening a pull request back against it.

Please use it that way. A fork is a route back here, not a place to take the
project somewhere else. It must keep the license intact and must not be promoted,
distributed, or offered to anyone as a separate work. If you want to build on UIA
in a way the licenses do not permit, open an issue or email info@mycorx.com.au.
A separate written agreement is available.

## Prohibited actions

Under both licenses, you may NOT:

- Publish or distribute a derivative work
- Embed the code in a product or service you provide to anyone else
- Sell or sublicense the code
- Host the code as a service

## Branding

MycorX and U.I.A. names, logos, and icons are not covered by the code license.
See [TRADEMARKS.md](TRADEMARKS.md) before using them anywhere, including in a fork
you run yourself.

## Copyright headers

Every `.rs` and `.ts` source file carries a two-line MycorX copyright header.
CI enforces this. To add it to new files:

    ./scripts/check-copyright-headers.sh --fix

## Adding or changing a dependency

NOTICE.txt lists the license of every crate and npm package UIA ships. It is
generated, not hand-edited, so regenerate and commit it in the same PR as any
Cargo.toml, Cargo.lock, or package.json change:

    pnpm install            # NOTICE needs an installed npm store
    ./scripts/gen-notice.sh

CI enforces this in the `notice` job, which fails if any dependency in the graph
is missing from NOTICE.txt. `./scripts/gen-notice.sh --check` runs the same check
locally.

The check asks whether everything is attributed, not whether the file matches byte
for byte. Four npm packages are platform-specific, so an entry in NOTICE.txt that
is not installed on your machine is reported as a note rather than an error — that
is another platform's binary, and over-attributing costs nothing.

You can regenerate on any OS. `pnpm licenses list` only sees the store your
machine installed, so no single platform can render the npm half correctly — a
Linux run cannot see the win32 binaries and a Windows run cannot see the linux
ones. The default is therefore a merge: it keeps the npm entries already in
NOTICE.txt that your machine has no way of seeing, and prints which ones it
carried. The crate half is rewritten every time, because `cargo metadata` runs
without `--filter-platform` and is already the union everywhere.

That means removals need saying out loud. To drop a package on purpose:

    ./scripts/gen-notice.sh --rewrite

which regenerates from scratch and keeps only what is installed locally. Run it
on Linux and read the diff.

The generator fails on any dependency that declares no license, rather than
omitting it. If that happens, read the package's own LICENSE/COPYING file and
record what you find in `LICENSE_OVERRIDES` in scripts/gen_notice.py.

Copyleft dependencies need a second look. UIA currently ships some MPL-2.0
packages, which is fine as long as they stay unmodified; anything stronger (GPL,
AGPL, LGPL where the exception does not apply) should be raised in an issue before
it is merged.

---

Thank you for respecting the MycorX licensing terms.
