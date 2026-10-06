# Security Policy

## Reporting a Vulnerability

Please report security vulnerabilities privately through
[GitHub Security Advisories](https://github.com/mycorx/user-interface-agents/security/advisories/new)
rather than opening a public issue.

If you're unable to use GitHub Security Advisories, email
security@mycorx.com.au with a description of the issue.

We aim to acknowledge reports within 5 business days and to keep you
updated on remediation progress until the issue is resolved.

## Scope

In scope:

- The UIA application code in this repository (Rust backend, Svelte/Tauri
  frontend).
- The MCP manifest validation and handshake flow described in
  [`docs/MCP.md`](docs/MCP.md).

Out of scope:

- Third-party MCP servers a user chooses to add — UIA validates and
  sandboxes what it can, but it does not vet the servers themselves.
- Third-party dependencies (report those upstream; see
  [NOTICE.txt](NOTICE.txt) for what UIA builds on).

## Supported Versions

UIA does not yet have a stable release line. Security fixes are applied
to the `main` branch only.

## Disclosure

UIA is source-available under a dual PolyForm license (see
[LICENSE.md](LICENSE.md)), not open source, and there is no bug bounty
program. We ask for coordinated disclosure: please give us a reasonable
window to address a reported issue before any public discussion of it.
