<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { open } from '@tauri-apps/plugin-dialog';
  import { openUrl } from '@tauri-apps/plugin-opener';

  // Mirrors `uia_app::mcp_registry::LocalServerEntry` and `main.rs`'s
  // `RemoteServerSummary` across the IPC boundary, duplicated the same way
  // General's `AgentSettings` is. `has_header` rather than the value: the
  // header is a bearer token and the backend deliberately never returns it.
  // Note there is deliberately no `auth`/`oauth_client_id` field here:
  // `RemoteServerSummary` itself carries neither, so the client-side list of
  // already-added servers cannot tell which ones use OAuth. `client_id`
  // would be fine to show (public under PKCE, not a secret) but it is
  // genuinely absent from what the backend returns.
  type LocalServerEntry = { name: string; version: string | null; enabled: boolean };
  type DeclaredTool = { name: string; description: string };
  type RemoteServer = {
    name: string;
    url: string;
    header_name: string | null;
    has_header: boolean;
    enabled: boolean;
    declared_name: string | null;
    declared_version: string | null;
    tools: DeclaredTool[];
  };

  // Mirrors `main.rs`'s `PreviewOutcome`, `#[serde(tag = "kind")]` over a
  // newtype `Connected(RemoteServerSummary)` variant and a
  // `NeedsAuthorization { authorize_url }` struct variant — so a `Connected`
  // value serializes as `{ kind: "Connected", ...RemoteServerSummary fields
  // }`, not as a nested object. `RemoteAuth` (`mcp_registry.rs`) is a
  // data-less enum with `#[serde(rename = "static"/"oauth")]`, so it crosses
  // the IPC boundary as the bare string `"static"` / `"oauth"`, not `{
  // Static: null }`.
  type Auth = 'static' | 'oauth';
  type PreviewOutcome = ({ kind: 'Connected' } & RemoteServer) | { kind: 'NeedsAuthorization'; authorize_url: string };

  // Local and remote servers are listed together, so a row has to say which
  // half of the registry it came from — the kind decides which commands its
  // controls call, and it is the one distinction that matters for trust.
  type Row =
    | { kind: 'local'; name: string; entry: LocalServerEntry }
    | { kind: 'remote'; name: string; entry: RemoteServer };

  // Mirrors `session::ServerHealth`, a `#[serde(tag = "state")]` enum, so it
  // arrives as `{ state: "connected" }` / `{ state: "no_tools", message }` /
  // `{ state: "failed", message }`.
  type Health =
    | { state: 'connected' }
    | { state: 'no_tools'; message: string }
    | { state: 'failed'; message: string };

  // Mirrors `main.rs`'s `LocalServerDetails`. `env_keys`, not the values —
  // the backend deliberately never returns those.
  type LocalServerDetails = {
    version: string | null;
    command: string;
    args: string[];
    env_keys: string[];
  };

  // Mirrors `uia_app::mcp_registry::ConfigFieldView`. A sensitive field
  // never has `value`; `is_set` is all the backend will say about it.
  type ConfigField = {
    key: string;
    kind: string;
    title: string;
    description: string | null;
    required: boolean;
    sensitive: boolean;
    multiple: boolean;
    min: number | null;
    max: number | null;
    default: string | null;
    value: string | null;
    is_set: boolean;
  };

  // What the Status column shows. `pending` is the honest answer for a
  // server the running session never attempted — it is neither healthy nor
  // broken, and showing either would be a lie.
  type Status =
    | { kind: 'off' }
    | { kind: 'pending' }
    | { kind: 'connected' }
    | { kind: 'no-tools'; message: string }
    | { kind: 'failed'; message: string };

  // Every change here is restart-gated: `session::build_executor` reads the
  // registry once at startup, so nothing below affects the running session.
  let { onchange }: { onchange: () => void } = $props();

  let localServers = $state<LocalServerEntry[]>([]);
  let remotes = $state<RemoteServer[]>([]);

  // One list, sorted by name, because the name is what has to be unique
  // across both kinds — `add_local_server`/`add_remote` each refuse a name
  // the other already holds, so sorting by it puts any near-collision
  // side by side.
  const rows = $derived<Row[]>(
    [
      ...localServers.map((entry) => ({ kind: 'local' as const, name: entry.name, entry })),
      ...remotes.map((entry) => ({ kind: 'remote' as const, name: entry.name, entry })),
    ].sort((a, b) => a.name.localeCompare(b.name)),
  );

  // `null` is the list; anything else is the install flow standing in front
  // of it. 'choose' is the local-or-remote fork, 'remote' the form — the
  // local branch never gets a step of its own because the OS file dialog IS
  // its step.
  let wizard = $state<null | 'choose' | 'remote'>(null);

  // Written once per session start by `build_executor`, so this is a
  // snapshot of the session that is RUNNING, not of the registry above it.
  // A name missing from here was never attempted by that session.
  let health = $state<Record<string, Health>>({});
  // Resolved lazily, per local server, on first expand — a filesystem read
  // each, so fetching them all on mount would stat every installed bundle
  // just to draw the list. `string` is the failure reason.
  let localDetails = $state<Record<string, LocalServerDetails | string>>({});
  // Per local server: its settings schema (`string` is the reason it could
  // not be read), the text currently in each input, and the last save outcome.
  let configFields = $state<Record<string, ConfigField[] | string>>({});
  let configDraft = $state<Record<string, Record<string, string>>>({});
  let configNote = $state<Record<string, { ok: boolean; text: string }>>({});

  let installError = $state<string | null>(null);
  let installing = $state(false);
  let remoteError = $state<string | null>(null);

  // The two-step add: `preview` holds what the server declared about
  // itself, and until it is non-null there is nothing to confirm. This is
  // the whole point of the flow — a remote server has no bundle to inspect,
  // so its own declaration is the only thing there is to review.
  let formName = $state('');
  let formUrl = $state('');
  let formHeaderName = $state('');
  let formHeaderValue = $state('');
  let showHeaderFields = $state(false);
  let authMode = $state<Auth>('static');
  let formOauthClientId = $state('');
  let preview = $state<RemoteServer | null>(null);
  let connecting = $state(false);
  let expanded = $state<string | null>(null);

  // The OAuth half of the add flow. There is no Tauri event for "the
  // background token exchange finished" (nothing in Tasks 1-7 wires one up),
  // so the only way to find out is retrying `preview_mcp_remote_server` and
  // seeing whether it now says `Connected` — `checkSignIn()` below is that
  // retry, triggered by the user rather than polled automatically. Polling
  // was considered and rejected: every `NeedsAuthorization` response spawns
  // a fresh, non-deduplicated background wait and a fresh browser tab
  // (`main.rs`'s `complete_oauth_sign_in` — a known, deliberately deferred
  // gap from Task 7), so an automatic timer would multiply that, not just
  // poll harmlessly. A manual button keeps the number of spawned sessions
  // equal to the number of times the user actually asked.
  let authorizing = $state(false);
  let checkingSignIn = $state(false);
  let authorizeUrl = $state<string | null>(null);
  // The name a standing `authorizing` wait belongs to — so the
  // `uia://oauth-sign-in-error` listener below can tell "the background
  // exchange for THIS wait failed" apart from a late error for one the user
  // already abandoned (Cancel, or editing the form and retrying).
  let authorizingName = $state<string | null>(null);
  // Set when `NeedsAuthorization` arrives for a name that is already in
  // `remotes` and enabled. `RemoteServerSummary` has no `auth` field, so
  // this is a proxy rather than a direct signal — but a `Static` server
  // never produces `NeedsAuthorization` in the first place (only the OAuth
  // branch of `preview_remote` does), so seeing it for a name that already
  // connected once before is, in practice, exactly the "the refresh token
  // is no longer valid" case the spec calls "session expired".
  let sessionExpired = $state(false);
  // `add_remote` refuses a duplicate name outright (no upsert), so an
  // already-added OAuth server can never be re-added through this form.
  // What re-signing-in through this form DOES do is drive a fresh token
  // exchange for the same account, which lands in the same keychain entry
  // `KeyringCredentialStore` looks up by server identity — so the already-
  // saved entry benefits from the refreshed token the next time a session
  // starts, with no registry write needed. `reconnectedNoAdd` marks that
  // case so the form offers "Done" instead of a "Confirm & add" that would
  // just fail with `DuplicateName`.
  let reconnectedNoAdd = $state(false);

  const canConnect = $derived(
    formName.trim() !== '' &&
      formUrl.trim() !== '' &&
      (authMode !== 'oauth' || formOauthClientId.trim() !== ''),
  );

  function refreshLocalServers() {
    invoke<LocalServerEntry[]>('list_mcp_local_servers')
      .then((v) => (localServers = v))
      .catch((e) => console.error('list_mcp_local_servers invoke() failed:', e));
  }

  function refreshRemotes() {
    invoke<RemoteServer[]>('list_mcp_remote_servers')
      .then((v) => (remotes = v))
      .catch((e) => console.error('list_mcp_remote_servers invoke() failed:', e));
  }

  // The background OAuth exchange (`main.rs`'s `complete_oauth_sign_in`) has
  // no caller left to report back to by the time it succeeds or fails — the
  // command that started it already returned `NeedsAuthorization` long
  // before this fires. Without this listener a failure there (a locked
  // keychain, the token exchange itself rejecting) left the form stuck on
  // "Waiting for you to finish signing in…" forever, with nothing telling
  // the user it had already failed.
  listen<{ name: string; message: string }>('uia://oauth-sign-in-error', (event) => {
    // Ignore an error for a wait this form isn't showing any more — Cancel,
    // or editing the form and retrying, already abandoned it.
    if (event.payload.name !== authorizingName) return;
    authorizing = false;
    authorizingName = null;
    authorizeUrl = null;
    remoteError = event.payload.message;
  }).catch((e) => console.error('uia://oauth-sign-in-error listen() failed:', e));

  function refreshHealth() {
    invoke<Record<string, Health>>('list_mcp_server_health')
      .then((v) => (health = v))
      .catch((e) => console.error('list_mcp_server_health invoke() failed:', e));
  }

  // Enabled is the first question: a server the user turned off was never
  // attempted, so it has no health to report and must not read as broken.
  // Only then does the session's snapshot get a say, and its ABSENCE is the
  // interesting case — see `Status`.
  function statusFor(row: Row): Status {
    if (!row.entry.enabled) return { kind: 'off' };
    const h = health[row.name];
    if (!h) return { kind: 'pending' };
    if (h.state === 'connected') return { kind: 'connected' };
    // Hyphenated here, underscored on the wire: the kind is also a CSS class.
    if (h.state === 'no_tools') return { kind: 'no-tools', message: h.message };
    return { kind: 'failed', message: h.message };
  }

  async function toggleDetails(row: Row) {
    // A note describes the last save of an earlier visit; drop it on both
    // expand and collapse. Not in `loadConfig`, which runs right after a save
    // and would erase the "Saved" it just set.
    delete configNote[row.name];
    if (expanded === row.name) {
      expanded = null;
      return;
    }
    expanded = row.name;
    // Remote details are already in hand from `list_mcp_remote_servers`;
    // only a local server has to be resolved from disk. Re-fetched on every
    // expand rather than cached for the visit: the whole value of this panel
    // is that it reports what is on disk NOW, and a bundle can change
    // underneath a Settings window left open.
    if (row.kind !== 'local') return;
    try {
      localDetails[row.name] = await invoke<LocalServerDetails>('describe_mcp_local_server', {
        name: row.name,
      });
    } catch (e) {
      localDetails[row.name] = String(e);
    }
    await loadConfig(row.name);
  }

  // Decided from the LOADED field, never the live draft, so the control does
  // not change type while the user edits. A string setting whose DECLARED
  // default is exactly `true`/`false` is a switch in disguise; it still stores
  // that string. A stored value alone never makes a toggle: a switch cannot
  // show "auto" or empty, so a free-text setting (default "auto", stored
  // "true") would become impossible to set back.
  //
  // The reverse also holds: a stored value that is neither empty nor
  // `true`/`false` (hand-edited `uia-mcp.json`, an older build) cannot be shown
  // by a switch — it would read as "off" while the server is handed the odd
  // text, and the next save would overwrite it unseen. Such a field falls back
  // to a text box showing the real value, and becomes a switch again on the
  // load after it is set back to `true`/`false`. `loadConfig` re-reads every
  // time a row is expanded, so this is re-evaluated then, not only at restart.
  function isToggleField(f: ConfigField): boolean {
    if (f.value != null && f.value !== '' && f.value !== 'true' && f.value !== 'false') return false;
    if (f.kind === 'boolean') return true;
    if (f.kind !== 'string' || f.sensitive || f.multiple) return false;
    return f.default === 'true' || f.default === 'false';
  }

  async function loadConfig(name: string) {
    try {
      const fields = await invoke<ConfigField[]>('get_mcp_local_server_config', { name });
      configFields[name] = fields;
      // A sensitive input starts empty: leaving it empty means "keep".
      configDraft[name] = Object.fromEntries(
        fields.map((f) => [f.key, f.sensitive ? '' : (f.value ?? f.default ?? '')]),
      );
    } catch (e) {
      configFields[name] = String(e);
    }
  }

  async function saveConfig(name: string) {
    const fields = configFields[name];
    if (typeof fields === 'string' || fields === undefined) return;
    // A key left out of `values` is unchanged; `null` clears it.
    const values: Record<string, string | null> = {};
    for (const f of fields) {
      // Every value goes over IPC as a string (the command takes strings), so
      // coerce whatever the draft holds.
      let text = String(configDraft[name][f.key] ?? '');
      if (f.kind === 'number') text = text.trim();
      if (f.sensitive && text === '') continue; // keep the stored secret
      if (f.kind === 'number' && text !== '' && !Number.isFinite(Number(text))) {
        configNote[name] = { ok: false, text: `${f.title}: "${text}" is not a number` };
        return;
      }
      if (!f.sensitive && text === (f.default ?? null)) {
        // Shown as a default, not chosen: store nothing (and clear any
        // previously stored value).
        values[f.key] = null;
      } else if (text === '' && isToggleField(f) && f.default === null) {
        // An unset toggle displays unchecked, so that is what it saves as.
        values[f.key] = 'false';
      } else {
        values[f.key] = text === '' ? null : text;
      }
    }
    try {
      await invoke('set_mcp_local_server_config', { name, values });
      configNote[name] = { ok: true, text: 'Saved. Restart UIA to apply.' };
      onchange();
      await loadConfig(name);
    } catch (e) {
      configNote[name] = { ok: false, text: String(e) };
    }
  }

  async function clearSecret(name: string, key: string) {
    try {
      await invoke('set_mcp_local_server_config', { name, values: { [key]: null } });
      configNote[name] = { ok: true, text: 'Removed. Restart UIA to apply.' };
      onchange();
    } catch (e) {
      configNote[name] = { ok: false, text: String(e) };
    }
    await loadConfig(name);
  }

  async function pickPath(name: string, key: string, directory: boolean) {
    const chosen = await open({ directory, multiple: false });
    if (typeof chosen === 'string') configDraft[name][key] = chosen;
  }

  refreshLocalServers();
  refreshRemotes();
  refreshHealth();

  function openWizard() {
    installError = null;
    remoteError = null;
    wizard = 'choose';
  }

  // Leaves the install flow and drops everything it was holding. The remote
  // form's own state has to go with it: a standing `authorizing` wait, a
  // fetched `preview`, and a half-typed bearer token must not still be there
  // the next time Install is pressed.
  function closeWizard() {
    resetForm();
    wizard = null;
  }

  async function installLocalServer() {
    installError = null;
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: 'MCP Bundle', extensions: ['mcpb'] }],
      });
      // The dialog was dismissed — stay on the fork rather than closing the
      // whole flow, so cancelling the file picker doesn't also undo the
      // choice of "local".
      if (!selected || typeof selected !== 'string') return;
      installing = true;
      await invoke<LocalServerEntry>('install_mcp_local_server', { sourcePath: selected });
      refreshLocalServers();
      onchange();
      closeWizard();
    } catch (e) {
      installError = String(e);
      console.error('install_mcp_local_server invoke() failed:', e);
    } finally {
      installing = false;
    }
  }

  async function toggleLocalServer(entry: LocalServerEntry) {
    installError = null;
    try {
      await invoke('set_mcp_local_server_enabled', { name: entry.name, enabled: entry.enabled });
      onchange();
    } catch (e) {
      installError = String(e);
      entry.enabled = !entry.enabled;
    }
  }

  async function removeLocalServer(name: string) {
    installError = null;
    if (!confirm(`Remove the ${name} server? Its installed files are deleted.`)) return;
    try {
      await invoke('remove_mcp_local_server', { name });
      refreshLocalServers();
      onchange();
    } catch (e) {
      installError = String(e);
      console.error('remove_mcp_local_server invoke() failed:', e);
    }
  }

  // The confirm step must only ever be reachable for a declaration fetched
  // with exactly the request that will be sent by `confirmAdd()` — that is
  // the entire promise of the two-step flow ("nothing is saved until you
  // confirm what it declared"). So any edit to any field that feeds
  // `headerArgs()`/the name/the URL has to drop the standing preview,
  // forcing a fresh Connect before Confirm & add can be reached again.
  function invalidatePreview() {
    preview = null;
    // Editing any field the request is built from invalidates a standing
    // `authorizing` wait too, same reasoning as the preview it already
    // invalidated: the browser tab that's open corresponds to whatever
    // name/URL/client-id was submitted, not to whatever is in the form now.
    authorizing = false;
    authorizingName = null;
    sessionExpired = false;
    reconnectedNoAdd = false;
    authorizeUrl = null;
  }

  function setAuthMode(mode: Auth) {
    authMode = mode;
    if (mode === 'oauth') {
      // Task 2's backend guard refuses an OAuth server that also sets a
      // static header, so don't even offer the field.
      showHeaderFields = false;
      formHeaderName = '';
      formHeaderValue = '';
    } else {
      formOauthClientId = '';
    }
    invalidatePreview();
  }

  function headerArgs() {
    if (authMode === 'oauth') return { headerName: null, headerValue: null };
    const name = formHeaderName.trim();
    const value = formHeaderValue;
    return name === ''
      ? { headerName: null, headerValue: null }
      : { headerName: name, headerValue: value };
  }

  function authArgs() {
    return {
      auth: authMode,
      oauthClientId: authMode === 'oauth' ? formOauthClientId.trim() : null,
    };
  }

  function openAuthorizeUrl(url: string) {
    openUrl(url).catch((e) => console.error('failed to open the sign-in page:', e));
  }

  // Shared by the initial Connect and the manual "check again" retry — both
  // send the exact same request and react to the outcome the same way.
  // `attemptedName` is captured by the caller before the async call so a
  // mid-flight edit to `formName` can't change which server the outcome is
  // attributed to.
  function handlePreviewOutcome(outcome: PreviewOutcome, attemptedName: string) {
    if (outcome.kind === 'Connected') {
      // Checked directly against `remotes`, not `authorizing && sessionExpired`:
      // an already-signed-in OAuth entry's still-valid access token can reach
      // `Connected` WITHOUT ever passing through the `NeedsAuthorization`
      // branch below (the one place that used to set `sessionExpired`), which
      // otherwise made this look like a fresh add and sent `confirmAdd` into
      // `add_remote`'s `DuplicateName` refusal for a name that was already
      // there the whole time.
      const wasReconnect = remotes.some((r) => r.name === attemptedName && r.enabled);
      const { kind: _kind, ...server } = outcome;
      preview = server;
      authorizing = false;
      authorizingName = null;
      authorizeUrl = null;
      reconnectedNoAdd = wasReconnect;
    } else {
      sessionExpired = remotes.some((r) => r.name === attemptedName && r.enabled);
      authorizeUrl = outcome.authorize_url;
      authorizing = true;
      authorizingName = attemptedName;
      openAuthorizeUrl(outcome.authorize_url);
    }
  }

  function resetForm() {
    formName = '';
    formUrl = '';
    formHeaderName = '';
    formHeaderValue = '';
    formOauthClientId = '';
    showHeaderFields = false;
    authMode = 'static';
    preview = null;
    authorizing = false;
    authorizingName = null;
    checkingSignIn = false;
    sessionExpired = false;
    reconnectedNoAdd = false;
    authorizeUrl = null;
  }

  // There is no backend command to cancel a pending OAuth session —
  // `PendingOAuthSessions` (`mcp_registry.rs`) only supports `store`/`take`
  // from Rust, nothing reachable from the frontend. So this only abandons
  // the FORM's own wait; the background `complete_oauth_sign_in` task
  // `main.rs` spawned keeps listening for the browser round trip until
  // `OAUTH_BROWSER_TIMEOUT` (5 minutes) elapses, then gives up on its own.
  // That self-expiry is what makes it safe to just drop the frontend state
  // here rather than blocking Cancel on a round trip that doesn't exist.
  function cancelAuthorizing() {
    closeWizard();
  }

  async function connectAndDeclare() {
    remoteError = null;
    preview = null;
    reconnectedNoAdd = false;
    connecting = true;
    const attemptedName = formName.trim();
    try {
      const outcome = await invoke<PreviewOutcome>('preview_mcp_remote_server', {
        name: attemptedName,
        url: formUrl.trim(),
        ...headerArgs(),
        ...authArgs(),
      });
      handlePreviewOutcome(outcome, attemptedName);
    } catch (e) {
      remoteError = String(e);
    } finally {
      connecting = false;
    }
  }

  async function checkSignIn() {
    remoteError = null;
    checkingSignIn = true;
    const attemptedName = formName.trim();
    try {
      const outcome = await invoke<PreviewOutcome>('preview_mcp_remote_server', {
        name: attemptedName,
        url: formUrl.trim(),
        ...headerArgs(),
        ...authArgs(),
      });
      handlePreviewOutcome(outcome, attemptedName);
    } catch (e) {
      remoteError = String(e);
    } finally {
      checkingSignIn = false;
    }
  }

  // Already-added OAuth servers just refreshed their keychain credential by
  // signing in again; there's nothing left to save. See `reconnectedNoAdd`'s
  // declaration for why `add_mcp_remote_server` would only fail here.
  function finishReconnect() {
    closeWizard();
    refreshRemotes();
    onchange();
  }

  async function confirmAdd() {
    remoteError = null;
    const attemptedName = formName.trim();
    try {
      const outcome = await invoke<PreviewOutcome>('add_mcp_remote_server', {
        name: attemptedName,
        url: formUrl.trim(),
        ...headerArgs(),
        ...authArgs(),
      });
      if (outcome.kind === 'NeedsAuthorization') {
        // The credential the preview step stored didn't survive to confirm
        // (e.g. it expired in the gap) — treat it like a fresh
        // authorization request rather than surfacing this as an error.
        handlePreviewOutcome(outcome, attemptedName);
        return;
      }
      closeWizard();
      refreshRemotes();
      onchange();
    } catch (e) {
      remoteError = String(e);
    }
  }

  async function toggleRemote(server: RemoteServer) {
    remoteError = null;
    try {
      await invoke('set_mcp_remote_server_enabled', {
        name: server.name,
        enabled: server.enabled,
      });
      onchange();
    } catch (e) {
      remoteError = String(e);
      server.enabled = !server.enabled;
    }
  }

  async function removeRemote(name: string) {
    remoteError = null;
    if (!confirm(`Remove the remote server ${name}?`)) return;
    try {
      await invoke('remove_mcp_remote_server', { name });
    } catch (e) {
      remoteError = String(e);
    } finally {
      // In `finally`, not only on success: the command can now fail AFTER
      // the registry write, when deleting the saved sign-in is what went
      // wrong. Refreshing only on success would leave the list showing a
      // server that is already gone, next to an error about it.
      refreshRemotes();
      onchange();
    }
  }
</script>

<section class="group">
  {#if wizard === null}
    <h2>Servers</h2>
    <p class="hint">
      Every server here is off until you enable it, and each one is trusted on
      its own.
    </p>

    {#if rows.length === 0}
      <p class="empty">No servers yet.</p>
    {:else}
      <div class="table-scroll">
        <table class="servers">
        <thead>
          <tr>
            <th scope="col" class="col-enabled">Enabled</th>
            <th scope="col">Name</th>
            <th scope="col">Status</th>
            <th scope="col"><span class="sr-only">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          {#each rows as row (row.kind + ':' + row.name)}
            {@const status = statusFor(row)}
            <tr>
              <td class="col-enabled">
                {#if row.kind === 'local'}
                  <input
                    type="checkbox"
                    aria-label={`Enable ${row.name}`}
                    bind:checked={row.entry.enabled}
                    onchange={() => toggleLocalServer(row.entry)}
                  />
                {:else}
                  <input
                    type="checkbox"
                    aria-label={`Enable ${row.name}`}
                    bind:checked={row.entry.enabled}
                    onchange={() => toggleRemote(row.entry)}
                  />
                {/if}
              </td>
              <td class="entry-name">
                <span class="name-text">{row.name}</span>
                <span class="badge badge-{row.kind}">{row.kind === 'local' ? 'Local' : 'Remote'}</span>
              </td>
              <td>
                <span class="status status-{status.kind}">
                  <span class="dot" aria-hidden="true"></span>
                  {#if status.kind === 'connected'}Connected
                  {:else if status.kind === 'no-tools'}No tools
                  {:else if status.kind === 'failed'}Failed
                  {:else if status.kind === 'pending'}Pending restart
                  {:else}Off{/if}
                </span>
              </td>
              <td class="actions">
                <!-- A caret rather than the words: "Show details" cost ~72px
                     of a row that has to fit five columns, and the meaning
                     survives in the aria-label and the title tooltip. -->
                <button
                  type="button"
                  class="details-toggle"
                  aria-expanded={expanded === row.name}
                  aria-label={expanded === row.name
                    ? `Hide details for ${row.name}`
                    : `Show details for ${row.name}`}
                  onclick={() => toggleDetails(row)}
                >
                  Details
                  <svg class="chev" class:open={expanded === row.name} viewBox="0 0 24 24" aria-hidden="true">
                    <polyline points="9 6 15 12 9 18" />
                  </svg>
                </button>
                <button
                  type="button"
                  class="danger"
                  onclick={() =>
                    row.kind === 'local' ? removeLocalServer(row.name) : removeRemote(row.name)}
                >
                  Remove
                </button>
              </td>
            </tr>

            {#if status.kind === 'failed' || status.kind === 'no-tools'}
              <!-- Its own row, always visible: the reason is a sentence, not
                   a cell, and burying it behind Show details would leave the
                   list looking healthy while a server is broken. A server
                   that connected and then contributed nothing gets the same
                   line — it is just as absent from the session. -->
              <tr class="warn-row">
                <td colspan="4">
                  <p class="warn">
                    <svg class="warn-glyph" viewBox="0 0 24 24" aria-hidden="true">
                    <path d="M12 3.5 22 20.5H2z" />
                    <line x1="12" y1="10" x2="12" y2="14.5" />
                    <line x1="12" y1="17.5" x2="12" y2="17.6" />
                  </svg>
                    {status.message}
                  </p>
                </td>
              </tr>
            {/if}

            {#if expanded === row.name}
              <tr class="details-row">
                <td colspan="4">
                  {#if row.kind === 'remote'}
                    <dl class="details">
                      <dt>URL</dt>
                      <dd>{row.entry.url}{row.entry.has_header ? ' · authenticated' : ''}</dd>
                      <dt>Declared</dt>
                      <dd>
                        {row.entry.declared_name ?? 'undeclared'}{row.entry.declared_version
                          ? ` v${row.entry.declared_version}`
                          : ''}
                      </dd>
                    </dl>
                    <p class="hint">
                      Declared {row.entry.tools.length} tool{row.entry.tools.length === 1 ? '' : 's'}
                      when it was added:
                    </p>
                    <ul class="tools">
                      {#each row.entry.tools as tool (tool.name)}
                        <li><code>{tool.name}</code> &mdash; {tool.description}</li>
                      {/each}
                    </ul>
                  {:else if localDetails[row.name] === undefined}
                    <p class="hint">Reading the bundle&hellip;</p>
                  {:else if typeof localDetails[row.name] === 'string'}
                    <p class="warn">
                      <svg class="warn-glyph" viewBox="0 0 24 24" aria-hidden="true">
                      <path d="M12 3.5 22 20.5H2z" />
                      <line x1="12" y1="10" x2="12" y2="14.5" />
                      <line x1="12" y1="17.5" x2="12" y2="17.6" />
                  </svg>
                      {localDetails[row.name]}
                    </p>
                  {:else}
                    {@const d = localDetails[row.name] as LocalServerDetails}
                    <dl class="details">
                      <dt>Version</dt>
                      <dd>{d.version ?? 'none declared'}</dd>
                      <dt>Runs</dt>
                      <dd><code>{d.command}</code></dd>
                      {#if d.args.length > 0}
                        <dt>Args</dt>
                        <dd><code>{d.args.join(' ')}</code></dd>
                      {/if}
                      {#if d.env_keys.length > 0}
                        <dt>Env</dt>
                        <dd><code>{d.env_keys.join(', ')}</code></dd>
                      {/if}
                    </dl>
                    <p class="hint">
                      Re-checked every launch, not just at install &mdash; this is what
                      would run now.
                    </p>
                    {#if Array.isArray(configFields[row.name]) && (configFields[row.name] as ConfigField[]).length > 0}
                      {@const cfgFields = configFields[row.name] as ConfigField[]}
                      <p class="declaration-head"><strong>Configuration</strong></p>
                      {#each cfgFields as f (f.key)}
                        {@const inputId = `cfg-${row.name}-${f.key}`}
                        <div class="field">
                          <label for={inputId}>{f.title}{f.required ? ' *' : ''}</label>
                          {#if isToggleField(f)}
                            <input
                              id={inputId}
                              type="checkbox"
                              role="switch"
                              checked={configDraft[row.name]?.[f.key] === 'true'}
                              onchange={(e) =>
                                (configDraft[row.name][f.key] = e.currentTarget.checked
                                  ? 'true'
                                  : 'false')}
                            />
                          {:else if f.kind === 'directory' || f.kind === 'file'}
                            <div class="cfg-row">
                              <input id={inputId} type="text" bind:value={configDraft[row.name][f.key]} />
                              <button
                                type="button"
                                onclick={() => pickPath(row.name, f.key, f.kind === 'directory')}
                              >
                                Browse&hellip;
                              </button>
                            </div>
                          {:else}
                            <div class="cfg-row">
                              <input
                                id={inputId}
                                type={f.sensitive ? 'password' : 'text'}
                                inputmode={f.kind === 'number' ? 'decimal' : undefined}
                                autocomplete="off"
                                placeholder={f.sensitive && f.is_set
                                  ? '•••••••• (saved — type to replace)'
                                  : ''}
                                bind:value={configDraft[row.name][f.key]}
                              />
                              {#if f.sensitive && f.is_set}
                                <button type="button" onclick={() => clearSecret(row.name, f.key)}>
                                  Remove
                                </button>
                              {/if}
                            </div>
                          {/if}
                          {#if f.description}<p class="hint">{f.description}</p>{/if}
                        </div>
                      {/each}
                      <div class="save-row">
                        <button type="button" onclick={() => saveConfig(row.name)}>
                          Save settings
                        </button>
                      </div>
                      {#if configNote[row.name]}
                        <p class={configNote[row.name].ok ? 'hint' : 'field-error'}>
                          {configNote[row.name].text}
                        </p>
                      {/if}
                    {:else if typeof configFields[row.name] === 'string'}
                      <p class="hint">Settings unavailable: {configFields[row.name]}</p>
                    {/if}
                  {/if}
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
        </table>
      </div>
    {/if}


    <div class="save-row">
      <button type="button" onclick={openWizard}>Install…</button>
    </div>
    {#if installError}
      <p class="field-error">{installError}</p>
    {/if}
    <p class="hint">Applies the next time the assistant restarts.</p>

  {:else if wizard === 'choose'}
    <div class="wizard-head">
      <button type="button" class="link back" onclick={closeWizard}>
        <svg class="back-glyph" viewBox="0 0 24 24" aria-hidden="true">
          <polyline points="15 6 9 12 15 18" />
        </svg>
        Back
      </button>
      <h2>Install a server</h2>
    </div>

    <ul class="choices">
      <li>
        <button type="button" class="choice" onclick={installLocalServer} disabled={installing}>
          <span class="choice-title">{installing ? 'Installing…' : 'Local'}</span>
          <span class="choice-body">
            Runs on this machine, from a <code>.mcpb</code> bundle you pick. Only
            bundles shipping a compiled executable are accepted &mdash; one
            carrying an interpreter and source is refused, because what runs
            should be reviewable before it runs.
          </span>
        </button>
      </li>
      <li>
        <button type="button" class="choice" onclick={() => (wizard = 'remote')}>
          <span class="choice-title">Remote</span>
          <span class="choice-body">
            Runs somewhere else, reached over HTTP at a URL you give. It is
            contacted first and must say what it is and what tools it offers.
          </span>
        </button>
      </li>
    </ul>
    {#if installError}
      <p class="field-error">{installError}</p>
    {/if}

  {:else}
    <div class="wizard-head">
      <button type="button" class="link back" onclick={() => (wizard = 'choose')}>
        <svg class="back-glyph" viewBox="0 0 24 24" aria-hidden="true">
          <polyline points="15 6 9 12 15 18" />
        </svg>
        Back
      </button>
      <h2>Install a remote server</h2>
    </div>
    <p class="hint">
      The server is contacted first and must say what it is and what tools it
      offers. Nothing is saved until you confirm what it declared.
    </p>

    <div class="field">
      <label for="mcp_remote_name">Name</label>
      <input
        id="mcp_remote_name"
        type="text"
        placeholder="docs"
        bind:value={formName}
        oninput={invalidatePreview}
        disabled={authorizing}
      />
      <p class="hint">Letters, digits, <code>-</code> and <code>_</code> only. Prefixes every tool name this server offers.</p>
    </div>

    <div class="field">
      <label for="mcp_remote_url">URL</label>
      <input
        id="mcp_remote_url"
        type="text"
        placeholder="https://example.com/mcp"
        bind:value={formUrl}
        oninput={invalidatePreview}
        disabled={authorizing}
      />
    </div>

    <fieldset class="field auth-mode-field">
      <legend>Authentication</legend>
      <div class="auth-mode">
        <label class="radio-label">
          <input
            type="radio"
            name="mcp_remote_auth"
            checked={authMode === 'static'}
            disabled={authorizing}
            onchange={() => setAuthMode('static')}
          />
          Static header
        </label>
        <label class="radio-label">
          <input
            type="radio"
            name="mcp_remote_auth"
            checked={authMode === 'oauth'}
            disabled={authorizing}
            onchange={() => setAuthMode('oauth')}
          />
          Sign in with a browser
        </label>
      </div>
      <p class="hint">
        {#if authMode === 'oauth'}
          Opens the server's sign-in page in your browser. Nothing is pasted
          here — the token is stored in your OS keychain, not this form.
        {:else}
          A single header, sent with every request. The classic <code>-H
          "Authorization: Bearer …"</code> shape.
        {/if}
      </p>
    </fieldset>

    {#if authMode === 'static'}
      <div class="field">
        <button
          type="button"
          class="link"
          onclick={() => {
            showHeaderFields = !showHeaderFields;
            if (!showHeaderFields) {
              // "Remove auth header" has to actually remove it. `headerArgs()`
              // keys off a non-empty name, not off this flag, so leaving the
              // values behind would keep sending a token the user just told us
              // to drop — to a third-party URL, on the next Connect, and into
              // the saved entry on Confirm & add.
              formHeaderName = '';
              formHeaderValue = '';
            }
            invalidatePreview();
          }}
        >
          {showHeaderFields ? 'Remove auth header' : 'Add an auth header…'}
        </button>
        {#if showHeaderFields}
          <div class="header-fields">
            <input
              type="text"
              placeholder="Authorization"
              bind:value={formHeaderName}
              oninput={invalidatePreview}
            />
            <input
              type="password"
              placeholder="Bearer …"
              bind:value={formHeaderValue}
              oninput={invalidatePreview}
            />
          </div>
          <p class="hint">Sent with every request to this server. Stored locally next to your config.</p>
        {/if}
      </div>
    {:else}
      <div class="field">
        <label for="mcp_remote_client_id">OAuth client ID</label>
        <input
          id="mcp_remote_client_id"
          type="text"
          placeholder="OAuth client ID"
          bind:value={formOauthClientId}
          oninput={invalidatePreview}
          disabled={authorizing}
        />
        <p class="hint">
          The public client id issued for this app by the server's OAuth
          provider (e.g. an Amazon Cognito app client). Not a secret under PKCE.
        </p>
      </div>
    {/if}

    {#if authorizing}
      <div class="declaration">
        <p class="declaration-head">
          {#if sessionExpired}
            <strong>Session expired — sign in again</strong>
          {:else}
            <strong>Waiting for you to finish signing in…</strong>
          {/if}
        </p>
        <p class="hint">
          We opened your browser to the sign-in page. Finish signing in there,
          then come back and check again.
        </p>
        {#if authorizeUrl}
          <p class="hint">
            <button type="button" class="link" onclick={() => openAuthorizeUrl(authorizeUrl!)}>
              Reopen the sign-in page
            </button>
          </p>
        {/if}
      </div>
    {:else if reconnectedNoAdd}
      <div class="declaration">
        <p class="declaration-head"><strong>Signed in.</strong></p>
        <p class="hint">
          {formName.trim()} was already added — this refreshed its saved
          credential, so there's nothing left to save.
        </p>
      </div>
    {:else if preview}
      <div class="declaration">
        <p class="declaration-head">
          <strong>{preview.declared_name ?? preview.name}</strong>
          {#if preview.declared_version}<span class="entry-meta">v{preview.declared_version}</span>{/if}
        </p>
        <p class="hint">This server declares {preview.tools.length} tool{preview.tools.length === 1 ? '' : 's'}:</p>
        <ul class="tools">
          {#each preview.tools as tool (tool.name)}
            <li><code>{tool.name}</code> &mdash; {tool.description}</li>
          {/each}
        </ul>
      </div>
    {/if}

    <div class="save-row">
      {#if authorizing}
        <button type="button" onclick={checkSignIn} disabled={checkingSignIn}>
          {checkingSignIn ? 'Checking…' : "I've signed in — check again"}
        </button>
        <button type="button" class="link" onclick={cancelAuthorizing}>Cancel</button>
      {:else if reconnectedNoAdd}
        <button type="button" onclick={finishReconnect}>Done</button>
      {:else if preview}
        <button type="button" onclick={confirmAdd}>Confirm &amp; add</button>
        <button type="button" class="link" onclick={closeWizard}>Cancel</button>
      {:else}
        <button type="button" onclick={connectAndDeclare} disabled={connecting || !canConnect}>
          {connecting ? 'Connecting…' : 'Connect'}
        </button>
      {/if}
    </div>
    {#if remoteError}
      <p class="field-error">{remoteError}</p>
    {/if}
  {/if}
</section>

<style>
  .group {
    margin-bottom: 24px;
    padding-bottom: 20px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }

  .group:last-of-type {
    border-bottom: none;
    margin-bottom: 0;
    padding-bottom: 0;
  }

  .group h2 {
    margin: 0 0 14px;
    font-size: 13px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: #8890a0;
  }

  /* Back sits on the same line as the heading, so leaving the flow reads as
     one step back rather than as another control in the panel. */
  .wizard-head {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 14px;
  }

  .wizard-head h2 {
    margin: 0;
  }

  /* An icon-and-label back button, not an underlined 12px text link: the
     chevron is SVG (a text "‹" sits off the label's baseline and centre), and
     the underline only appears on hover so the control reads as navigation. */
  button.link.back {
    flex: none;
    display: inline-flex;
    align-items: center;
    gap: 2px;
    font-size: 13px;
    text-decoration: none;
  }

  button.link.back:hover {
    text-decoration: underline;
  }

  .back-glyph {
    width: 16px;
    height: 16px;
    margin-left: -4px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .choices {
    list-style: none;
    margin: 0 0 12px;
    padding: 0;
    display: grid;
    gap: 10px;
  }

  .choice {
    display: block;
    width: 100%;
    text-align: left;
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.04);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 10px;
    padding: 12px 14px;
    cursor: pointer;
  }

  .choice:not(:disabled):hover {
    border-color: var(--accent, #6ea8fe);
    background: color-mix(in srgb, var(--accent, #6ea8fe) 8%, transparent);
  }

  .choice:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .choice-title {
    display: block;
    font-weight: 600;
    margin-bottom: 4px;
  }

  .choice-body {
    display: block;
    font-size: 12px;
    color: #8890a0;
  }

  .field {
    margin-bottom: 16px;
  }

  .field label {
    display: block;
    font-weight: 500;
    margin-bottom: 6px;
  }

  .field input[type='text'],
  .field input[type='password'],
  .header-fields input {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 10px;
  }

  /* A native checkbox drawn as a switch: keyboard and checked semantics stay. */
  .field input[role='switch'] {
    appearance: none;
    position: relative;
    width: 32px;
    height: 18px;
    margin: 0;
    border-radius: 9px;
    background: rgba(255, 255, 255, 0.18);
    cursor: pointer;
    transition: background 0.15s ease;
  }

  .field input[role='switch']::before {
    content: '';
    position: absolute;
    top: 2px;
    left: 2px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: #fff;
    transition: transform 0.15s ease;
  }

  .field input[role='switch']:checked {
    background: var(--accent, #6ea8fe);
  }

  .field input[role='switch']:checked::before {
    transform: translateX(14px);
  }

  .field input[role='switch']:focus-visible {
    outline: 2px solid var(--accent, #6ea8fe);
    outline-offset: 2px;
  }

  @media (prefers-reduced-motion: reduce) {
    .field input[role='switch'],
    .field input[role='switch']::before {
      transition: none;
    }
  }

  .header-fields {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
    margin-top: 8px;
  }

  /* Name and kind share a cell. The badge already reads "Local"/"Remote", so
     a column headed TYPE above it was saying the same thing twice -- and its
     header, not its content, was setting a ~68px floor the panel could not
     afford. */
  .entry-name {
    font-weight: 500;
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }

  .name-text {
    /* Wraps rather than widening the table. `anywhere` because a server name
       has no spaces to break on. */
    overflow-wrap: anywhere;
    min-width: 0;
  }

  .entry-meta {
    font-size: 12px;
    color: #8890a0;
  }

  /* The one distinction that matters for trust, so it is a chip rather than
     another line of grey meta text. */
  .badge {
    flex: none;
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    padding: 2px 6px;
    border-radius: 5px;
    border: 1px solid transparent;
  }

  .badge-local {
    color: #8fd19e;
    background: rgba(143, 209, 158, 0.12);
    border-color: rgba(143, 209, 158, 0.3);
  }

  .badge-remote {
    color: var(--accent, #6ea8fe);
    background: color-mix(in srgb, var(--accent, #6ea8fe) 12%, transparent);
    border-color: color-mix(in srgb, var(--accent, #6ea8fe) 30%, transparent);
  }

  /* Overflow is the table's problem, never the panel's. Server names and
     resolved launch paths are unbounded, so no window width settles this on
     its own — this is what keeps a long one from pushing the whole settings
     card sideways. */
  .table-scroll {
    overflow-x: auto;
    margin: 0 0 14px;
    /* The row is built to fit, so this should not appear -- but content here
       is unbounded (a long server name, a long failure message), so when it
       does appear it has to belong to this panel rather than being the OS's
       default light slab across a dark card. */
    scrollbar-width: thin;
    scrollbar-color: rgba(255, 255, 255, 0.22) transparent;
  }

  .table-scroll::-webkit-scrollbar {
    height: 8px;
  }

  .table-scroll::-webkit-scrollbar-track {
    background: transparent;
  }

  .table-scroll::-webkit-scrollbar-thumb {
    background: rgba(255, 255, 255, 0.22);
    border-radius: 4px;
  }

  .table-scroll::-webkit-scrollbar-thumb:hover {
    background: rgba(255, 255, 255, 0.34);
  }

  .servers {
    width: 100%;
    border-collapse: collapse;
  }

  .servers th {
    text-align: left;
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #8890a0;
    padding: 0 8px 6px 0;
    border-bottom: 1px solid rgba(255, 255, 255, 0.12);
    white-space: nowrap;
  }

  .servers td {
    padding: 8px 8px 8px 0;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
    vertical-align: middle;
  }

  /* The toggle column holds the control and nothing else, so it is sized to
     the control rather than to a label. */
  .col-enabled {
    width: 1%;
    padding-right: 14px !important;
    white-space: nowrap;
  }

  .servers input[type='checkbox'] {
    accent-color: var(--accent, #6ea8fe);
    margin: 0;
  }

  .actions {
    text-align: right;
    white-space: nowrap;
  }

  /* Carries the same chrome as Remove beside it. A bare caret had no border
     or background, so it read as a stray glyph in the gap between Status and
     Remove rather than as something to press -- the button frame is what
     makes it look clickable, not the size of the mark inside it. The word
     stays because 520px has room for it; the caret alongside is then only a
     state indicator, not the sole affordance. */
  .details-toggle {
    margin-right: 8px;
  }

  .chev {
    width: 12px;
    height: 12px;
    margin-left: 2px;
    vertical-align: -1px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2.25;
    stroke-linecap: round;
    stroke-linejoin: round;
    opacity: 0.75;
    transition: transform 0.15s ease;
  }

  .chev.open {
    transform: rotate(90deg);
  }

  @media (prefers-reduced-motion: reduce) {
    .chev {
      transition: none;
    }
  }

  .warn-glyph {
    flex: none;
    width: 14px;
    height: 14px;
    margin-top: 2px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .status {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    white-space: nowrap;
  }

  .status .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: currentColor;
    flex: none;
  }

  /* Never colour alone: each state carries its own word, so the column still
     reads correctly without colour vision. */
  .status-connected {
    color: #8fd19e;
  }

  .status-failed {
    color: #e8776f;
  }

  /* Amber, not red: it connected, so this is not the same failure as a
     server that never arrived — but it is not healthy either. */
  .status-no-tools {
    color: #e0b458;
  }

  .status-pending {
    color: #e0b458;
  }

  .status-off {
    color: #8890a0;
  }

  /* No bottom border: the warning belongs to the row above it, and a rule
     between them would read as a separate entry. */
  .warn-row td {
    border-bottom: none;
    padding-top: 0;
  }

  .warn {
    margin: 0;
    font-size: 12px;
    color: #e8776f;
    display: flex;
    gap: 6px;
    align-items: baseline;
    /* The reason is the point of this line, so it wraps to as many lines as
       it needs. A `resource_metadata="..."` URL has no spaces to break on. */
    overflow-wrap: anywhere;
    min-width: 0;
  }

  .details-row td {
    padding-top: 0;
    color: #b8bcc6;
  }

  .details {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 2px 12px;
    margin: 0 0 8px;
    font-size: 12px;
  }

  .details dt {
    color: #8890a0;
  }

  .details dd {
    margin: 0;
    overflow-wrap: anywhere;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }

  .tools {
    list-style: none;
    margin: 6px 0 0;
    padding: 0 0 0 26px;
    font-size: 12px;
    color: #b8bcc6;
  }

  .tools li {
    padding: 2px 0;
    /* Tool descriptions are written by the server, not by us, so they can
       carry an unbroken URL or identifier of any length. */
    overflow-wrap: anywhere;
  }

  .declaration {
    margin: 12px 0;
    padding: 10px 12px;
    background: color-mix(in srgb, var(--accent, #6ea8fe) 8%, transparent);
    border: 1px solid color-mix(in srgb, var(--accent, #6ea8fe) 24%, transparent);
    border-radius: 8px;
  }

  .declaration-head {
    margin: 0 0 4px;
  }

  .empty {
    margin: 0 0 12px;
    font-size: 12px;
    color: #8890a0;
  }

  /* A settings input with its Browse / Remove button beside it. */
  .cfg-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .save-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .save-row button,
  .cfg-row button,
  .actions button {
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 12px;
    cursor: pointer;
  }

  .save-row button:not(:disabled):hover {
    background: var(--accent, #6ea8fe);
    color: #0c0e12;
    border-color: var(--accent, #6ea8fe);
  }

  .save-row button:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .actions button:not(.danger):hover {
    border-color: var(--accent, #6ea8fe);
    color: #9ec5ff;
  }

  .danger:hover {
    background: #d9534f !important;
    color: #fff !important;
    border-color: #d9534f !important;
  }

  button.link {
    font: inherit;
    font-size: 12px;
    color: var(--accent, #6ea8fe);
    background: none;
    border: none;
    padding: 0;
    text-decoration: underline;
    cursor: pointer;
  }

  .auth-mode-field {
    border: none;
    padding: 0;
    margin: 0 0 16px;
  }

  .auth-mode-field legend {
    display: block;
    font-weight: 500;
    margin-bottom: 6px;
    padding: 0;
  }

  .auth-mode {
    display: flex;
    gap: 16px;
    margin-top: 4px;
  }

  .radio-label {
    display: flex;
    align-items: center;
    gap: 6px;
    font-weight: 400;
    cursor: pointer;
  }

  .radio-label input {
    accent-color: var(--accent, #6ea8fe);
  }

  .hint {
    margin: 6px 0 0;
    font-size: 12px;
    color: #8890a0;
  }

  .field-error {
    margin: 6px 0 0;
    font-size: 12px;
    color: #d9534f;
  }
</style>
