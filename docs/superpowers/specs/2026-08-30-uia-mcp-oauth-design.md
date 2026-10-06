# uia: OAuth 2.1 authorization for remote MCP servers

**Date:** 2026-08-30
**Status:** implemented and shipped
**Scope:** the uia desktop client. What a server must provide to work with it is described under "What a server must provide".

## The problem

A remote MCP server that protects its endpoint with short-lived OAuth access tokens
could not be used from uia for more than about an hour. The only supported way to
authenticate was to paste a token into a static `Authorization` header. An access token
typically lives about an hour, so the connection died mid-session and the only cure was to
delete the server and paste a fresh token by hand. That makes the remote MCP transport
unusable in production for any server that issues expiring tokens.

## Root cause

Not a missing refresh function: a missing place to put one.

`crates/uia-mcp/src/client.rs::http_transport_config` bakes the configured headers into
`StreamableHttpClientTransportConfig.custom_headers` once, at connect time. That map is
frozen for the life of the transport. A perfect token-refresh module would have nowhere
to inject the result.

So the structural fix, a token consulted *per request* rather than captured once, is
required regardless of which authorization scheme is used. Everything else in this
document follows from that.

## Decisions

### 1. OAuth 2.1 as the MCP Authorization spec defines it, not a provider adapter

The MCP Authorization spec *is* OAuth 2.1. `rmcp` 3.1.4, already a dependency, ships the
whole stack: RFC 7591 dynamic client registration, RFC 9728 protected-resource metadata
discovery, PKCE S256, and RFC 8707 resource indicators. Integrating it is wiring, not
implementing.

The alternative considered was a client for one specific identity provider's proprietary
login API (direct HTTPS calls, a password grant, proactive refresh at a 60s margin). It
was rejected as the primary path because it serves exactly one deployment inside a tool
whose README commits remote MCP to *any* HTTP(S) server. A provider-shaped special case
in a generic tool is the thing that rots.

### 2. Fix the server, don't special-case it in the client

Making a server a spec-compliant OAuth resource server costs a handful of lines on the
server (see "What a server must provide"). Teaching the client to speak one provider's
dialect costs far more and would need re-porting into every other client that ever wants
in. Fixing the server deletes the special case instead of adding one, and the client ends
up with **zero server-specific code**.

### 3. Single-layer authentication: no separate connection token

A two-layer design (a token for the client-to-MCP-server connection, plus a separate
token for the user) was considered and rejected on two grounds:

- **It creates a second identity.** A connection token able to reach the server
  independently of the operator's own sign-in is exactly the kind of credential that
  weakens an access-control model: it can be used without the user being present.
- **It answers a question already answered.** The split between "which app" and "which
  human" is already enforced from one token, because the server can check the client id
  and the username as separate assertions.

Two tokens would also mean two expiries and two refresh paths: two ways to fail, when the
bug being fixed is that one of them fails.

The "pop a browser, get a token, refresh it" behaviour is not a second layer. It is what
the single-layer MCP OAuth flow already does.

*Revisit if* a server ever needs to act on a third-party service on the user's behalf.
Then the downstream service has its own identity system and the layered "MCP server as
OAuth proxy" pattern becomes correct. This design does not block that.

### 4. Pre-configured `client_id` where the provider offers no dynamic registration

Some authorization servers (Amazon Cognito is one) expose no RFC 7591 registration
endpoint; creating a client is an authenticated admin API, not a public `/register`.
`rmcp` supports a pre-configured `client_id`, so uia works with them: the user enters the
public client id in the add-server form.

The cost is that clients which cannot take a manual `client_id` cannot self-onboard to
such a server. Closing that would mean a shim in front of the authorization server
implementing `/register`. Out of scope here; noted as the known ceiling.

### 5. Custom-scheme callback via `tauri-plugin-deep-link`

`uia://callback`, not a loopback listener. Some authorization servers exact-match
callback URLs and do not honour RFC 8252's "any port for loopback" rule, so a loopback
design needs a fixed port that can collide. A custom scheme avoids that and works on
mobile unchanged.

**Re-examined after a desktop spike, and confirmed.** The original wording justified this
as "the only option that works on mobile", and mobile turned out to be planned but not yet
started, so the premise was checked again rather than inherited. Loopback was reconsidered
on its merits and rejected: mobile is on the roadmap (after desktop stabilises), which
makes a loopback listener *known* throwaway work; the app is built per platform
regardless, so per-platform registration is not the burden it appeared to be; and
loopback's one real advantage, testability in a headless Linux environment, is largely
recovered by decision 5a below.

#### 5a. The callback splits into a pure function and a thin glue layer

Required, not stylistic. It is what keeps the untestable surface small and the mechanism
swappable when mobile arrives:

- **Pure:** `parse_callback(url) -> (code, state)` plus validation of the CSRF `state`
  against the pending authorization session. No `tauri`, no OS, no network: unit testable
  anywhere, and where the real logic and the real bugs live. The same discipline that
  already keeps `mcp_registry` free of `tauri`.
- **Glue:** receives a URL and calls the pure function. Two entry points, because the
  platforms differ (see below). A handful of lines with nothing branching worth testing.

Put the mechanism behind a `CallbackListener` seam
(`async fn await_callback(&self, expected_state) -> Result<AuthCode>`) so deep-link and
loopback are two implementations of one trait. Where the authorization server permits
several callback URLs per client, adding `http://localhost:PORT/callback` beside
`uia://callback` later is a single configuration change.

Deliberately **not** shipping both as a dev/prod split: two live paths and two failure
modes, where the dev one stops reflecting production exactly when it matters.

#### 5b. Desktop spike findings

Verified against `tauri-plugin-deep-link` 2.4.9 source with Tauri 2.11.5:

| Finding | Consequence |
|---|---|
| **`on_open_url` does not fire on Linux or Windows.** The OS spawns a *new instance* with the URL as argv (plugin README). | Fatal for OAuth without mitigation: the code lands in a second process while the PKCE verifier and CSRF state live in the first. **Requires `tauri-plugin-single-instance` 2.4.3 with feature `deep-link`**, which forwards argv to the running instance. A second dependency, not one. |
| **macOS cannot register at runtime**: `register()` returns `UnsupportedPlatform`. | The scheme must be declared in `tauri.conf.json` (baked into Info.plist) and works only for a bundled `.app`, not under `tauri dev`. |
| **Linux runtime registration shells out** to `xdg-mime` and `update-desktop-database`, writing a `.desktop` file to `~/.local/share/applications`. | Those binaries must exist on the target. |
| **`handle_cli_arguments` only matches schemes declared in the Tauri config**; dynamically registered schemes are explicitly skipped. | `uia` must live in `plugins.deep-link.desktop.schemes`; runtime `register()` alone is not enough. |
| **`handle_cli_arguments` ignores the URL unless it is the *sole* CLI argument.** | Any future CLI flag silently breaks deep links. Worth a comment at the call site. |
| **Not testable under WSL2**: `xdg-mime`, `update-desktop-database` and `xdg-open` are all absent, with no `XDG_CURRENT_DESKTOP`. | End-to-end verification is a **manual test on a real Windows or macOS desktop**, never something CI pretends to cover. |

`tauri-plugin-single-instance` is close to free here: preventing a second copy of a
tray-resident voice assistant, which would contend for the same hotkey and audio device,
is behaviour this app wants regardless.

Dependencies this adds, neither previously present: `tauri-plugin-deep-link` **2.4.9** and
`tauri-plugin-single-instance` **2.4.3** (feature `deep-link`).

### 6. Refresh token in the OS keychain

Via the existing `uia-app/src/secrets.rs` (keyring v3), **not** a plaintext credentials
file. Porting a command-line tool's plaintext token store verbatim would be a downgrade
from what uia already does.

To respect `scripts/check-core-deps.sh`, the trait lives in `uia-mcp` and the
keyring-backed implementation in `uia-app`, injected at connect time: the same shape as
the existing `SecretStore`.

### 7. Static headers stay supported

Plenty of MCP servers issue long-lived personal access tokens. The existing pasted-header
path remains as it was, and serves as the regression guard that the refactor preserves
current behaviour.

## Spike evidence

Run against a live Amazon Cognito user pool acting as the authorization server.

| Question | Result |
|---|---|
| Does the server reject RFC 8707's `resource` param? | **No.** `/authorize` returns 302 to login identically with and without it, and round-trips the param into the login redirect. `/token` returns the same `invalid_client` either way. |
| Does discovery work at rmcp's expected path? | **Yes.** HTTP 200 at `{issuer}/.well-known/openid-configuration`, rmcp's candidate #3 (path-appending). Issuer matches. |
| Is a hosted login domain available? | **Yes**, a prefix domain on the provider. No domain work needed. |
| Does rmcp accept metadata without `code_challenge_methods_supported`? | **Yes.** The provider omits the field; `rmcp` warns and proceeds with S256 (`auth.rs:1643-1648`). Had rmcp been strict, this would have been a hard stop. |
| Is `response_types_supported` compatible? | **Yes**, includes `code`, so `validate_server_metadata` passes. |
| Dynamic client registration? | **Absent**, as predicted: no `registration_endpoint` in the metadata. |

**Residual gap, closed.** The first `/token` evidence was strong but not airtight: it
could only reach `invalid_client`, because the only OAuth-enabled client in the pool was
confidential and no secret was being sent. The provider authenticates the client *before*
validating other parameters, so a rejection of `resource` could have hidden behind that.

Creating a public client closed it definitively. Against that client, `/token` with a
bogus code returns `{"error":"invalid_grant"}` **identically with and without** the
`resource` parameter. `invalid_grant` rather than `invalid_client` proves the provider
authenticated the client, parsed every parameter including the unknown `resource`, and
rejected only the code itself. `/authorize` likewise returned 302 to `/login` either way,
carrying the parameter forward.

The same exchange also confirmed the provider accepts the custom scheme `uia://callback`
with no `redirect_mismatch`, validating decision 5.

## What a server must provide

uia contains no code specific to any one server. A remote MCP server works with the
OAuth flow if it behaves as a standard OAuth 2.1 resource server:

**A. Advertise the challenge.** An unauthenticated request must get a 401 whose
`WWW-Authenticate` header carries the RFC 9728 pointer:

```
WWW-Authenticate: Bearer resource_metadata="{base}/.well-known/oauth-protected-resource"
```

**B. Serve the metadata document** at `/.well-known/oauth-protected-resource`,
unauthenticated, on the **root** of the origin, beside any health endpoints and not
inside the sub-application mounted at the MCP path:

```json
{
  "resource": "{base}/mcp/",
  "authorization_servers": ["https://<authorization-server-issuer>"]
}
```

Two details bite in practice:

- **Trailing slash.** If the MCP endpoint redirects the bare path to the slashed one, the
  `resource` value must keep the slash; a client that does not follow redirects treats
  the redirect as fatal.
- **Same origin, configured explicitly.** `{base}` is the server's own externally
  reachable origin, which only the server can state. It should be explicit
  configuration that fails closed at startup when missing: a resource document
  advertising the wrong origin sends clients to authenticate against the wrong place, and
  rmcp validates that the metadata URL is same-origin with the request.

**C. Register a public client** with the authorization server for uia:

- public (no secret): uia ships on users' machines, where a secret would not be one
- authorization-code flow with PKCE S256
- callback URL `uia://callback`
- scopes appropriate to the server (typically `openid email profile`)

The server's token verifier should accept that client id. Where the verifier already
accepts a list of client ids, this is configuration, not code.

**D. Everything else is unchanged.** Existing API routes and any command-line client keep
working as they do today.

## Architecture: the client

**A. The seam** (`uia-mcp`), the root-cause fix, required for any auth scheme:

```rust
#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn token(&self) -> Result<Option<String>, ToolError>;
}
```

`AuthedHttpClient<C>` wraps rmcp's `StreamableHttpClient`, consulting the provider on
every request rather than reusing a frozen header map.

`Option` is load-bearing in the return type: `None` means "send this request
unauthenticated", which is what drives the reactive model. The first request to an
unknown server goes out bare precisely so its 401 challenge can be read and answered.
Missing credentials are therefore not an error at this layer.

**B. Implementations.**

- `OAuthProvider` wraps rmcp's `AuthClient` / `AuthorizationManager`, which already
  provides both proactive refresh (`get_access_token` refreshes inside a buffer window)
  and reactive recovery (`call_reacting_to_challenges`: on 401, refresh once, retry once,
  then surface the challenge).

**Correction, found while planning the implementation.** An earlier draft of this section
proposed a `StaticHeader` *provider* carrying today's pasted header, as the regression
guard. That is wrong, and the code would not have worked.

rmcp's per-request `auth_header` parameter is applied by the reqwest client as
`request_builder.bearer_auth(value)`: it sets `Authorization: Bearer <value>` and takes the
**bare token**, not a full header value. The path is Bearer-only and Authorization-only.
uia's existing feature is deliberately more general: an arbitrary header *name* and
*value* (Claude Code's `-H`), which may be `X-API-Key: …` or `Authorization: Basic …`.

So the two mechanisms are not interchangeable, and the correct split is:

- **Arbitrary static headers stay in `custom_headers`**, exactly as before. Frozen at
  connect time is fine for them: a long-lived token has nothing to refresh. This is also
  why the refactor carries near-zero regression risk: that path is untouched.
- **`TokenProvider` supplies only the refreshable bearer token**, returning the bare token
  for rmcp to prefix.

There is therefore no `StaticHeader` provider; it would have been an abstraction with one
impossible implementation. The regression guard is instead that the existing
`custom_headers` tests keep passing unchanged.

**Constraint this creates:** a server must not use both mechanisms for `Authorization`.
If `custom_headers` carries an `Authorization` entry *and* a provider yields a token,
both would target the same header and one would silently win. A server is either
static-header or OAuth, never both, and the registry enforces that.

**C. Credential storage.** A `CredentialStore` implementation backed by `secrets.rs`,
keyed per server so two remote servers never share a token.

**D. Deep-link handling.** `tauri-plugin-deep-link` registers `uia://`; the callback
carries the authorization code back to the pending `AuthorizationSession`.

**E. `preview_remote` rework.** Previously a 401 surfaced as `RegistryError::Unreachable`,
so an authenticating server could never be added at all. The add-server flow becomes:

```
enter URL → connect → 401 + challenge → discover AS → browser → uia://callback
          → authorized → initialize + tools/list → show declaration → user approves → save
```

This preserves the existing rule that a remote server must declare itself live before it
can be trusted, while letting an unauthenticated client first learn *how* to
authenticate. The registry's `allow_remote` gate and per-server enable flag are unchanged
and still apply.

## Error handling

| Condition | Behaviour |
|---|---|
| Access token expired | Refreshed proactively inside the buffer window; never surfaces. |
| Token rejected mid-session (revoked) | One silent refresh, one retry, then the challenge surfaces as a re-auth prompt. |
| Refresh token expired (often ~30 days) | Not recoverable silently. Settings shows "session expired, sign in again". Without this the outage returns in a month, more mysteriously. |
| Keychain unavailable | Degrades to "not signed in" and prompts, matching `OsKeyring`'s existing error-to-`None` posture. Never crashes. |
| Browser flow abandoned | The pending session times out; the server stays unsaved. `PREVIEW_TIMEOUT` is the model. |
| Server offers no OAuth metadata | Falls back to the static-header path; unchanged behaviour for token-in-header servers. |

## Testing

TDD throughout: failing test first, per project convention.

**uia.** A `TokenProvider` fake proving the token is re-read per request, which is the
direct regression test for the frozen-header bug; the existing `custom_headers` tests kept
passing unchanged, which is what proves the arbitrary-static-header path did not regress;
the existing redaction tests extended so no token reaches `Debug` output through the new
types.

**Server side.** A compatible server should test, on its own side, that the 401 carries a
well-formed `resource_metadata` pointer, that the well-known document names the right
issuer and resource, that the route is reachable **unauthenticated** (a metadata document
behind auth is useless), and that a missing origin setting fails closed at startup.

**End to end.** Manual, in `docs/MANUAL-TEST.md`: add a server in Settings, complete the
browser flow, confirm the tools list, then confirm a session outliving the ~1h
access-token lifetime keeps working. That last check is the actual acceptance criterion;
it is the thing that fails without this work.

## Known deviations from the MCP spec

Some authorization servers, Amazon Cognito among them, do not implement every optional
piece. Documented rather than hidden:

1. **No dynamic client registration.** Clients need a pre-configured `client_id`.
2. **No true audience binding.** The server accepts RFC 8707's `resource` but does not
   honour it; access tokens carry `client_id`, not `aud`. This is why a resource server
   in front of such a provider has to check `client_id` by hand, and that check remains
   the binding.
3. **PKCE support undiscoverable.** The provider omits `code_challenge_methods_supported`.
   PKCE works; it just is not advertised. A stricter client than rmcp would refuse.

## Out of scope

- **A dynamic-registration shim in front of such a provider.** Considered and deferred.
  Because the client discovers the authorization server from *the provider's* metadata
  document, which the resource server does not control, a `/register` endpoint on the
  resource server is invisible unless it advertises itself as the authorization server
  and serves its own metadata.

  A cheaper hybrid exists: mixed metadata naming the resource server for
  `registration_endpoint` while leaving `authorization_endpoint` and `token_endpoint` on
  the provider, so the flow is never proxied. It works with rmcp, whose `require_issuer`
  defaults to `false`. But it is self-defeating: the point of registration is to admit
  *third-party* clients, and the trick that makes it cheap depends on those clients not
  validating the issuer. A stricter client than rmcp rejects it, which is precisely the
  client it was built for.

  The real cost is that dynamically registered clients cannot appear in the server's
  list of accepted client ids, so its verifier would have to relax to "any client in the
  pool", a weakening worth refusing. It also means an unauthenticated public endpoint
  creating provider resources, needing rate limiting and client quota management.

  Deferring is free: adding registration later changes only what the resource metadata
  advertises. Revisit when a concrete third-party client exists, at which point whether
  it validates issuers is a known fact rather than a guess.
- Multi-factor authentication challenges on the provider's side, which affect any
  command-line client that uses a password grant. An argument for moving such a client
  to this same browser flow, not a task here.

## Implementation phases

1. **Server** (outside this repository): challenge header, well-known route, public
   client registration. Small, and nothing in uia can be tested against a real server
   until it lands.
2. **Client seam:** `TokenProvider` + `AuthedHttpClient`. The root-cause fix; no behaviour
   change, fully covered by regression tests.
3. **Client OAuth:** rmcp `AuthClient`, keychain store, deep link, Settings states,
   `preview_remote` rework.
4. **Cleanup:** remove the paste-a-token instructions, update `uia.example.toml`, extend
   `MANUAL-TEST.md`.

Phases 1 and 2 are independent and may proceed in parallel. Phase 3 depends on both.
