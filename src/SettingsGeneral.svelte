<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import SettingsPersonas from './SettingsPersonas.svelte';
  import { describeUpdate, type UpdateSnapshot, type UpdateStatus } from './update';

  const MAX_PERSONAS = 6;

  // Mirrors `uia_app::settings::AgentSettings` - duplicated across the IPC
  // boundary like Providers'/Audio's own field types. Persona and avatar are
  // no longer here: both moved into the personas sidecar (`uia-personas.json`)
  // behind the Personas frame below.
  type AgentSettings = {
    name: string | null;
    always_on_top: boolean | null;
    quit_on_close: boolean | null;
    memory_enabled: boolean | null;
    home_location: string | null;
    auto_update_check: boolean | null;
  };

  // Restart-to-switch fields per FD5 (name, persona) fire this on save, same
  // as Providers/Audio - the parent shows one shared notice regardless of
  // which tab the change came from. Avatar, always-on-top and start-at-login
  // are excluded: none of them are baked into `build_session` (FD5's exact
  // list is secrets/endpoint/engine/model/name/persona), so they apply live.
  // Close behavior is the one lifecycle field FD5 leaves out of that list but
  // that `main()` still only resolves once at startup (see S5/main.rs), so it
  // fires the same notice as name/persona.
  let { onchange }: { onchange: () => void } = $props();

  let name = $state('');
  let alwaysOnTop = $state(false);
  let quitOnClose = $state(false);
  let memoryEnabled = $state(true);
  let startAtLogin = $state(false);
  let personasOpen = $state(false);
  let homeLocation = $state('');
  let autoUpdateCheck = $state(true);

  // Snapshot of what's actually persisted, refreshed on load and after every
  // successful save - the identity Save button is greyed out until name
  // differs from its snapshot. Persona no longer factors in here: it saves
  // itself, immediately, from inside the Personas frame.
  let savedName = $state('');
  let savedHomeLocation = $state('');
  const identityDirty = $derived(
    name.trim() !== savedName || homeLocation.trim() !== savedHomeLocation
  );

  let identitySaveState = $state<'idle' | 'saving' | 'saved'>('idle');
  let identityError = $state<string | null>(null);
  let lifecycleError = $state<string | null>(null);
  let startAtLoginError = $state<string | null>(null);

  invoke<AgentSettings>('get_agent_settings')
    .then((settings) => {
      name = settings.name ?? '';
      savedName = name;
      alwaysOnTop = settings.always_on_top ?? false;
      quitOnClose = settings.quit_on_close ?? false;
      // `null` means the toggle has never been set, so the backend's own
      // resolution against `[memory] backend` applies. Local is that
      // default, so an untouched install shows this on.
      memoryEnabled = settings.memory_enabled ?? true;
      homeLocation = settings.home_location ?? '';
      savedHomeLocation = homeLocation;
      autoUpdateCheck = settings.auto_update_check ?? true;
    })
    .catch((e) => console.error('get_agent_settings invoke() failed:', e));

  invoke<boolean>('get_start_at_login')
    .then((value) => {
      startAtLogin = value;
    })
    .catch((e) => console.error('get_start_at_login invoke() failed:', e));

  function currentAgentSettings(): AgentSettings {
    return {
      name: name.trim() === '' ? null : name.trim(),
      always_on_top: alwaysOnTop,
      quit_on_close: quitOnClose,
      memory_enabled: memoryEnabled,
      home_location: homeLocation.trim() === '' ? null : homeLocation.trim(),
      auto_update_check: autoUpdateCheck,
    };
  }

  async function persistAgentSettings() {
    await invoke('set_agent_settings', { settings: currentAgentSettings() });
  }

  async function toggleAutoUpdateCheck() {
    lifecycleError = null;
    try {
      await persistAgentSettings();
    } catch (e) {
      lifecycleError = String(e);
      console.error('set_agent_settings (auto_update_check) failed:', e);
      autoUpdateCheck = !autoUpdateCheck;
    }
  }

  let currentVersion = $state('');
  let update = $state<UpdateStatus>({ status: 'idle' });
  invoke<UpdateSnapshot>('get_update_status')
    .then((snapshot) => {
      currentVersion = snapshot.current_version;
      update = snapshot.status;
    })
    .catch((e) => console.error('get_update_status invoke() failed:', e));
  listen<UpdateStatus>('uia://update', (event) => {
    update = event.payload;
  }).catch((e) => console.error('uia://update listen() failed:', e));

  async function checkNow() {
    installError = '';
    try {
      update = await invoke<UpdateStatus>('check_for_update');
    } catch (e) {
      update = { status: 'failed', message: String(e) };
    }
  }

  // An offer turns the button into the install, so Settings never shows two
  // update buttons side by side. Rust refuses while the assistant is busy and
  // says so; that message is shown in place of the status.
  let installError = $state('');
  async function installNow() {
    installError = '';
    try {
      await invoke('install_update');
    } catch (e) {
      installError = String(e);
      console.error('install_update invoke() failed:', e);
    }
  }

  let updateBusy = $derived(update.status === 'checking' || update.status === 'downloading');
  let updateButtonLabel = $derived(
    update.status === 'checking'
      ? 'Checking…'
      : update.status === 'downloading'
        ? 'Downloading…'
        : update.status === 'available'
          ? `Install ${update.version}`
          : 'Check now'
  );

  async function saveIdentity() {
    identitySaveState = 'saving';
    identityError = null;
    try {
      await persistAgentSettings();
      savedName = name.trim();
      savedHomeLocation = homeLocation.trim();
      identitySaveState = 'saved';
      onchange();
      setTimeout(() => {
        identitySaveState = 'idle';
      }, 1500);
    } catch (e) {
      identityError = String(e);
      console.error('saveIdentity failed:', e);
      identitySaveState = 'idle';
    }
  }

  async function toggleAlwaysOnTop() {
    lifecycleError = null;
    const next = alwaysOnTop;
    try {
      await getCurrentWindow().setAlwaysOnTop(next);
      await persistAgentSettings();
    } catch (e) {
      lifecycleError = String(e);
      console.error('setAlwaysOnTop failed:', e);
      alwaysOnTop = !next;
    }
  }

  async function toggleQuitOnClose() {
    lifecycleError = null;
    try {
      await persistAgentSettings();
      onchange();
    } catch (e) {
      lifecycleError = String(e);
      console.error('set_agent_settings (quit_on_close) failed:', e);
      quitOnClose = !quitOnClose;
    }
  }

  async function toggleMemoryEnabled() {
    lifecycleError = null;
    try {
      await persistAgentSettings();
      onchange();
    } catch (e) {
      lifecycleError = String(e);
      console.error('set_agent_settings (memory_enabled) failed:', e);
      memoryEnabled = !memoryEnabled;
    }
  }

  async function toggleStartAtLogin() {
    startAtLoginError = null;
    const next = startAtLogin;
    try {
      await invoke('set_start_at_login', { enabled: next });
    } catch (e) {
      startAtLoginError = String(e);
      console.error('set_start_at_login invoke() failed:', e);
      startAtLogin = !next;
    }
  }
</script>

{#if personasOpen}
  <SettingsPersonas onclose={() => (personasOpen = false)} {onchange} />
{:else}
  <section class="group">
    <h2>Assistant identity</h2>
    <p class="hint">Name applies the next time the assistant restarts.</p>

    <div class="field">
      <label for="agent_name">Name</label>
      <input id="agent_name" type="text" placeholder="MyMy" bind:value={name} />
    </div>

    <div class="field">
      <label for="home_location">Home location</label>
      <input
        id="home_location"
        type="text"
        placeholder="e.g. Hobart, or -42.88,147.33"
        bind:value={homeLocation}
      />
      <p class="hint">
        Used as the default place for weather questions that don't name one,
        so the assistant doesn't have to ask. A place name or "lat,lon".
        Leave blank and it will ask instead of guessing. Applies the next
        time the assistant restarts.
      </p>
    </div>

    <div class="save-row">
      <button
        type="button"
        onclick={saveIdentity}
        disabled={identitySaveState === 'saving' || !identityDirty}
      >
        {identitySaveState === 'saving' ? 'Saving…' : 'Save'}
      </button>
      {#if identitySaveState === 'saved'}
        <span class="saved-notice">Saved.</span>
      {/if}
    </div>
    {#if identityError}
      <p class="field-error">{identityError}</p>
    {/if}

    <div class="field">
      <button type="button" class="choice" onclick={() => (personasOpen = true)}>
        Personas&hellip;
      </button>
      <p class="hint">
        Up to {MAX_PERSONAS} identities. The active one applies the next time the
        assistant restarts.
      </p>
    </div>
  </section>

  <section class="group">
    <h2>App behavior</h2>

    <div class="field">
      <label class="checkbox-label">
        <input type="checkbox" bind:checked={alwaysOnTop} onchange={toggleAlwaysOnTop} />
        Always on top
      </label>
      <p class="hint">Keeps the window above others. Applies immediately.</p>
    </div>

    <div class="field">
      <label class="checkbox-label">
        <input type="checkbox" bind:checked={quitOnClose} onchange={toggleQuitOnClose} />
        Close button quits instead of hiding to tray
      </label>
      <p class="hint">Applies the next time the assistant restarts.</p>
    </div>


    <div class="field">
      <label class="checkbox-label">
        <input type="checkbox" bind:checked={memoryEnabled} onchange={toggleMemoryEnabled} />
        Remember the conversation
      </label>
      <p class="hint">
        Keeps a transcript in <code>uia-conversation.jsonl</code> next to your config, and
        replays recent exchanges when the session reconnects. Nothing leaves this
        machine. Turn it off and the assistant starts each connection with no memory
        of what was said &mdash; including after an idle disconnect or a reconnect.
        Applies the next time the assistant restarts.
      </p>
      <p class="hint">
        Recording your own words means asking the engine to transcribe them, which
        adds a little latency and cost to each exchange.
      </p>
    </div>
    {#if lifecycleError}
      <p class="field-error">{lifecycleError}</p>
    {/if}

    <div class="field">
      <label class="checkbox-label">
        <input type="checkbox" bind:checked={startAtLogin} onchange={toggleStartAtLogin} />
        Start at system login
      </label>
      <p class="hint">Applies immediately.</p>
    </div>
    {#if startAtLoginError}
      <p class="field-error">{startAtLoginError}</p>
    {/if}

    <div class="field">
      <label class="checkbox-label">
        <input type="checkbox" bind:checked={autoUpdateCheck} onchange={toggleAutoUpdateCheck} />
        Check for updates automatically
      </label>
      <p class="hint">Applies immediately. "Check now" works either way.</p>
      <div class="update-row">
        <div class="update-status">
          <p class="update-version">Version {currentVersion}</p>
          <p class="hint" class:update-failed={update.status === 'failed' || installError}>
            {installError || describeUpdate(update)}
          </p>
        </div>
        <button
          type="button"
          class="choice"
          class:primary={update.status === 'available'}
          onclick={update.status === 'available' ? installNow : checkNow}
          disabled={updateBusy}
        >
          {updateButtonLabel}
        </button>
      </div>
    </div>
  </section>
{/if}

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

  .field {
    margin-bottom: 16px;
  }

  .field:last-child {
    margin-bottom: 0;
  }

  .field label {
    display: block;
    font-weight: 500;
    margin-bottom: 6px;
  }

  .field input[type='text'] {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 10px;
  }

  .save-row button,
  .choice {
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 12px;
    cursor: pointer;
  }

  .save-row button:not(:disabled):hover,
  .choice:hover {
    background: var(--accent, #6ea8fe);
    color: #0c0e12;
    border-color: var(--accent, #6ea8fe);
  }

  .choice:disabled,
  .save-row button:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .checkbox-label {
    display: flex !important;
    align-items: center;
    gap: 8px;
    font-weight: 500;
    cursor: pointer;
  }

  .checkbox-label input {
    accent-color: var(--accent, #6ea8fe);
  }

  .hint {
    margin: 6px 0 0;
    font-size: 12px;
    color: #8890a0;
  }

  /* Status on the left takes the free width and wraps; the button keeps its
     own column, so a long status never moves it or splits its line. */
  .update-row {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-top: 12px;
  }

  .update-status {
    flex: 1;
    min-width: 0;
  }

  .update-version {
    margin: 0;
    font-size: 13px;
  }

  .update-status .hint {
    margin-top: 2px;
    overflow-wrap: anywhere;
  }

  /* `.field-error`'s colour: a failed check or install is an error here. */
  .update-failed {
    color: #d9534f;
  }

  .update-row .choice {
    flex: none;
    white-space: nowrap;
  }

  .choice.primary {
    background: var(--accent, #6ea8fe);
    color: #0c0e12;
    border-color: var(--accent, #6ea8fe);
  }

  .save-row {
    display: flex;
    align-items: center;
    gap: 10px;
    /* The `.field` above supplies the gap over this row, but nothing supplied
       the one under it, so Save sat flush against the Personas button that
       follows -- two bordered controls touching, reading as one broken
       control rather than two. 16px is the rhythm every `.field` in this
       tab already uses. */
    margin-bottom: 16px;
  }

  .saved-notice {
    font-size: 12px;
    color: var(--accent, #6ea8fe);
  }

  .field-error {
    margin: 6px 0 0;
    font-size: 12px;
    color: #d9534f;
  }
</style>
