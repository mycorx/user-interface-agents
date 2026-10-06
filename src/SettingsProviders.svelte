<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';

  // Mirrors `uia_app::secrets::KNOWN_ACCOUNTS` — duplicated rather than
  // shared across the IPC boundary, matching how this window already
  // duplicates other small stable contracts (e.g. the engine id strings in
  // Hud.svelte) instead of importing Rust constants into TypeScript.
  type Account = 'openai_api_key' | 'aws_access_key_id' | 'aws_secret_access_key' | 'foundry_api_key';

  // Mirrors `uia_app::config::EngineChoice`'s wire strings (`as_wire`/
  // `from_str`). "bedrock" names the platform, so it reads alongside "openai"
  // and "foundry" rather than after the model family it runs (Nova Sonic).
  // The Rust side still accepts the old "nova" id, so a persisted engine
  // choice survives — but nothing here should write it.
  type EngineId = 'openai' | 'bedrock' | 'foundry';

  type Field = {
    account: Account;
    label: string;
    value: string;
    status: boolean | null; // null while the initial get_secret_status is in flight
    error: string | null;
  };

  // Fired on any successful save/clear, or an engine/model change - all of
  // it is restart-to-switch (FD5), so the parent shows one shared notice
  // regardless of which field or tab the change came from.
  let { onchange }: { onchange: () => void } = $props();

  // Restart-to-switch (FD5): reflects the engine this launch actually
  // started with until the radio is changed, at which point it becomes the
  // choice persisted for the *next* launch - `set_engine` never touches the
  // running session, same rule as `set_model` below.
  let engineChoice = $state<EngineId | null>(null);
  let engineError = $state<string | null>(null);

  invoke<string>('get_engine')
    .then((value) => {
      engineChoice = value as EngineId;
      // Independently expandable per-provider blocks (this stage's own
      // requirement) default to only the active one open, so the tab isn't
      // three walls of fields on first look - each still toggles on its own
      // after that.
      expanded = { openai: value === 'openai', bedrock: value === 'bedrock', foundry: value === 'foundry' };
    })
    .catch((e) => console.error('get_engine invoke() failed:', e));

  async function selectEngine(engine: EngineId) {
    const previous = engineChoice;
    engineChoice = engine;
    engineError = null;
    try {
      await invoke('set_engine', { engine });
      onchange();
    } catch (e) {
      engineChoice = previous;
      engineError = String(e);
      console.error('set_engine invoke() failed:', e);
    }
  }

  let expanded = $state<Record<EngineId, boolean>>({ openai: false, bedrock: false, foundry: false });

  function toggle(provider: EngineId) {
    expanded[provider] = !expanded[provider];
  }

  // One section per provider (not per model - OpenAI Realtime, AWS Bedrock's
  // Nova Sonic, and Microsoft AI Foundry are all named by provider), each
  // holding its own fields as sub-rows. The account ids underneath a
  // provider's fields are the canonical keystore ids (`aws_*` for AWS
  // Bedrock, matching AWS's own env-var convention) - only the grouping and
  // display labels are provider-facing.
  let openaiFields = $state<Field[]>([
    { account: 'openai_api_key', label: 'API Key', value: '', status: null, error: null },
  ]);

  let bedrockFields = $state<Field[]>([
    { account: 'aws_access_key_id', label: 'Access Key ID', value: '', status: null, error: null },
    {
      account: 'aws_secret_access_key',
      label: 'Secret Access Key',
      value: '',
      status: null,
      error: null,
    },
  ]);

  let foundryFields = $state<Field[]>([
    { account: 'foundry_api_key', label: 'API Key', value: '', status: null, error: null },
  ]);

  // Not a secret field (see `FoundrySection::endpoint`'s doc comment) - own
  // state and get/set commands rather than living in `foundryFields`, since
  // it has no OS-keystore "configured" status and shows its real value in a
  // plain text input instead of a password one.
  let foundryEndpoint = $state('');
  let savedFoundryEndpoint = $state('');
  let foundryEndpointError = $state<string | null>(null);
  const foundryEndpointDirty = $derived(foundryEndpoint.trim() !== savedFoundryEndpoint);

  async function refreshStatus(field: Field) {
    try {
      field.status = await invoke<boolean>('get_secret_status', { account: field.account });
    } catch (e) {
      console.error(`get_secret_status(${field.account}) invoke() failed:`, e);
    }
  }

  for (const field of [...openaiFields, ...bedrockFields, ...foundryFields]) {
    refreshStatus(field);
  }

  invoke<string | null>('get_foundry_endpoint')
    .then((value) => {
      foundryEndpoint = value ?? '';
      savedFoundryEndpoint = value ?? '';
    })
    .catch((e) => console.error('get_foundry_endpoint invoke() failed:', e));

  // FD3: OpenAI's and Bedrock's Voice Models are `<select>`s over verified
  // ids. Foundry's is a free-text input (a deployment name is whatever the
  // user typed into their own Azure portal, not a fixed public catalog id).
  //
  // BEDROCK_MODELS has one entry because AWS ships exactly one Nova Sonic
  // model. It is still a select, not the static label it used to be: the
  // control, command and config field are the same as the other engines', so
  // a second Sonic id is one entry here rather than a change of shape.
  const OPENAI_MODELS = ['gpt-realtime-2.1-mini', 'gpt-realtime-2.1'] as const;
  const BEDROCK_MODELS = ['amazon.nova-2-sonic-v1:0'] as const;

  let openaiModel = $state('');
  let openaiModelError = $state<string | null>(null);
  let bedrockModel = $state('');
  let bedrockModelError = $state<string | null>(null);
  let foundryModel = $state('');
  let savedFoundryModel = $state('');
  let foundryModelError = $state<string | null>(null);
  const foundryModelDirty = $derived(foundryModel.trim() !== savedFoundryModel);

  invoke<string>('get_model', { provider: 'openai' })
    .then((value) => {
      openaiModel = value;
    })
    .catch((e) => console.error("get_model('openai') invoke() failed:", e));

  invoke<string>('get_model', { provider: 'bedrock' })
    .then((value) => {
      bedrockModel = value;
    })
    .catch((e) => console.error("get_model('bedrock') invoke() failed:", e));

  invoke<string>('get_model', { provider: 'foundry' })
    .then((value) => {
      foundryModel = value;
      savedFoundryModel = value;
    })
    .catch((e) => console.error("get_model('foundry') invoke() failed:", e));

  async function saveOpenaiModel() {
    openaiModelError = null;
    try {
      await invoke('set_model', { provider: 'openai', value: openaiModel });
      onchange();
    } catch (e) {
      openaiModelError = String(e);
      console.error("set_model('openai') invoke() failed:", e);
    }
  }

  async function saveBedrockModel() {
    bedrockModelError = null;
    try {
      await invoke('set_model', { provider: 'bedrock', value: bedrockModel });
      onchange();
    } catch (e) {
      bedrockModelError = String(e);
      console.error("set_model('bedrock') invoke() failed:", e);
    }
  }

  async function saveFoundryModel() {
    foundryModelError = null;
    const trimmed = foundryModel.trim();
    if (trimmed === '') {
      foundryModelError = 'Enter a deployment name.';
      return;
    }
    try {
      await invoke('set_model', { provider: 'foundry', value: trimmed });
      savedFoundryModel = trimmed;
      onchange();
    } catch (e) {
      foundryModelError = String(e);
      console.error("set_model('foundry') invoke() failed:", e);
    }
  }

  // Mirrors `uia_app::settings::is_valid_http_url` - checked here first so
  // an obvious typo (missing scheme, stray whitespace) shows up instantly
  // instead of after a round trip; `set_foundry_endpoint` runs the same rule
  // again server-side as the actual guarantee.
  function isValidHttpUrl(value: string): boolean {
    if (/\s/.test(value)) return false;
    const lower = value.toLowerCase();
    const host = lower.startsWith('https://')
      ? lower.slice('https://'.length)
      : lower.startsWith('http://')
        ? lower.slice('http://'.length)
        : null;
    return host !== null && host.length > 0;
  }

  async function saveFoundryEndpoint() {
    foundryEndpointError = null;
    // An empty value is a deliberate clear (see `set_foundry_endpoint`'s
    // trim-to-`None` behavior), not a URL to validate.
    if (foundryEndpoint.trim() !== '' && !isValidHttpUrl(foundryEndpoint.trim())) {
      foundryEndpointError = 'Enter a valid http:// or https:// URL.';
      return;
    }
    try {
      await invoke('set_foundry_endpoint', { endpoint: foundryEndpoint });
      savedFoundryEndpoint = foundryEndpoint.trim();
      onchange();
    } catch (e) {
      foundryEndpointError = String(e);
      console.error('set_foundry_endpoint invoke() failed:', e);
    }
  }

  async function save(field: Field) {
    if (!field.value) return;
    field.error = null;
    try {
      await invoke('set_secret', { account: field.account, value: field.value });
      field.value = '';
      onchange();
      await refreshStatus(field);
    } catch (e) {
      field.error = String(e);
      console.error(`set_secret(${field.account}) invoke() failed:`, e);
    }
  }

  async function clear(field: Field) {
    if (!confirm(`Remove ${field.label} from this computer's secure storage?`)) return;
    field.error = null;
    try {
      await invoke('delete_secret', { account: field.account });
      onchange();
      await refreshStatus(field);
    } catch (e) {
      field.error = String(e);
      console.error(`delete_secret(${field.account}) invoke() failed:`, e);
    }
  }
</script>

{#snippet secretField(field: Field)}
  <div class="field">
    <div class="field-header">
      <label for={field.account}>{field.label}</label>
      <span class="status" class:configured={field.status === true}>
        {field.status === null ? 'Checking…' : field.status ? 'Configured' : 'Not configured'}
      </span>
    </div>
    <div class="field-controls">
      <input
        id={field.account}
        type="password"
        autocomplete="off"
        placeholder="Enter a new value to save"
        bind:value={field.value}
      />
      <button type="button" disabled={!field.value} onclick={() => save(field)}>Save</button>
      <button
        type="button"
        class="clear"
        disabled={field.status !== true}
        onclick={() => clear(field)}
      >
        Clear
      </button>
    </div>
    {#if field.error}
      <p class="field-error">{field.error}</p>
    {/if}
  </div>
{/snippet}

{#snippet providerHeader(provider: EngineId, title: string)}
  <button
    type="button"
    class="provider-header"
    aria-expanded={expanded[provider]}
    onclick={() => toggle(provider)}
  >
    <h2>{title}</h2>
    <svg class="chevron" class:open={expanded[provider]} viewBox="0 0 24 24" aria-hidden="true">
      <polyline points="9 6 15 12 9 18" />
    </svg>
  </button>
{/snippet}

<p class="intro">
  Credentials are written directly to this computer's secure storage (Windows Credential Manager,
  macOS Keychain, or Linux Secret Service) and are never sent anywhere else or shown again once
  saved.
</p>

<section class="active-provider">
  <h2>Active Provider</h2>
  <ul class="provider-radio-list">
    <li>
      <label>
        <input
          type="radio"
          name="engine"
          value="openai"
          checked={engineChoice === 'openai'}
          onchange={() => selectEngine('openai')}
        />
        OpenAI
      </label>
    </li>
    <li>
      <label>
        <input
          type="radio"
          name="engine"
          value="bedrock"
          checked={engineChoice === 'bedrock'}
          onchange={() => selectEngine('bedrock')}
        />
        AWS Bedrock
      </label>
    </li>
    <li>
      <label>
        <input
          type="radio"
          name="engine"
          value="foundry"
          checked={engineChoice === 'foundry'}
          onchange={() => selectEngine('foundry')}
        />
        Microsoft AI Foundry
      </label>
    </li>
  </ul>
  {#if engineError}
    <p class="field-error">{engineError}</p>
  {/if}
</section>

<section class="provider">
  {@render providerHeader('openai', 'OpenAI')}
  {#if expanded.openai}
    <div class="provider-body">
      <div class="field">
        <div class="field-header">
          <label for="openai_model">Voice Model</label>
        </div>
        <div class="field-controls">
          <select id="openai_model" bind:value={openaiModel} onchange={saveOpenaiModel}>
            {#each OPENAI_MODELS as id (id)}
              <option value={id}>{id}</option>
            {/each}
          </select>
        </div>
        {#if openaiModelError}
          <p class="field-error">{openaiModelError}</p>
        {/if}
      </div>
      {#each openaiFields as field (field.account)}
        {@render secretField(field)}
      {/each}
    </div>
  {/if}
</section>

<section class="provider">
  {@render providerHeader('bedrock', 'AWS Bedrock')}
  {#if expanded.bedrock}
    <div class="provider-body">
      <div class="field">
        <div class="field-header">
          <label for="bedrock_model">Voice Model</label>
        </div>
        <div class="field-controls">
          <select id="bedrock_model" bind:value={bedrockModel} onchange={saveBedrockModel}>
            {#each BEDROCK_MODELS as id (id)}
              <option value={id}>{id}</option>
            {/each}
          </select>
        </div>
        {#if bedrockModelError}
          <p class="field-error">{bedrockModelError}</p>
        {/if}
      </div>
      {#each bedrockFields as field (field.account)}
        {@render secretField(field)}
      {/each}
    </div>
  {/if}
</section>

<section class="provider">
  {@render providerHeader('foundry', 'Microsoft AI Foundry')}
  {#if expanded.foundry}
    <div class="provider-body">
      <div class="field">
        <div class="field-header">
          <label for="foundry_model">Voice Model</label>
        </div>
        <div class="field-controls">
          <input
            id="foundry_model"
            type="text"
            autocomplete="off"
            placeholder="Deployment name from your Azure portal"
            bind:value={foundryModel}
          />
          <button type="button" disabled={!foundryModelDirty} onclick={saveFoundryModel}>
            Save
          </button>
        </div>
        {#if foundryModelError}
          <p class="field-error">{foundryModelError}</p>
        {/if}
      </div>
      <div class="field">
        <div class="field-header">
          <label for="foundry_endpoint">Endpoint</label>
        </div>
        <div class="field-controls">
          <input
            id="foundry_endpoint"
            type="text"
            autocomplete="off"
            placeholder="https://your-resource.openai.azure.com"
            bind:value={foundryEndpoint}
          />
          <button type="button" disabled={!foundryEndpointDirty} onclick={saveFoundryEndpoint}>
            Save
          </button>
        </div>
        {#if foundryEndpointError}
          <p class="field-error">{foundryEndpointError}</p>
        {/if}
      </div>
      {#each foundryFields as field (field.account)}
        {@render secretField(field)}
      {/each}
    </div>
  {/if}
</section>

<style>
  .intro {
    margin: 0 0 24px;
    color: #b8bcc6;
    font-size: 13px;
  }

  .active-provider {
    margin-bottom: 24px;
    padding-bottom: 20px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }

  .active-provider h2 {
    margin: 0 0 12px;
    font-size: 13px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: #8890a0;
  }

  .provider-radio-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .provider-radio-list label {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
  }

  .provider-radio-list input[type='radio'] {
    accent-color: var(--accent, #6ea8fe);
  }

  .provider {
    margin-bottom: 12px;
    padding-bottom: 12px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }

  .provider:last-of-type {
    border-bottom: none;
    margin-bottom: 0;
    padding-bottom: 0;
  }

  .provider-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    width: 100%;
    font: inherit;
    color: inherit;
    background: none;
    border: none;
    padding: 8px 0;
    cursor: pointer;
    text-align: left;
  }

  .provider-header h2 {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: #8890a0;
  }

  .provider-header:hover h2 {
    color: #e8eaf0;
  }

  .chevron {
    width: 16px;
    height: 16px;
    flex: none;
    color: #8890a0;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
    transition: transform 0.15s ease;
  }

  .chevron.open {
    transform: rotate(90deg);
  }

  .provider-body {
    padding-top: 10px;
  }

  .field {
    margin-bottom: 16px;
  }

  .field:last-child {
    margin-bottom: 0;
  }

  .field-header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    margin-bottom: 6px;
  }

  .field-header label {
    font-weight: 500;
  }

  .status {
    font-size: 12px;
    color: #b8bcc6;
  }

  .status.configured {
    color: var(--accent, #6ea8fe);
  }

  .field-controls {
    display: flex;
    gap: 8px;
  }

  .field-controls input,
  .field-controls select {
    flex: 1;
    min-width: 0;
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 10px;
  }

  .field-controls select option {
    background: #14161c;
    color: #e8eaf0;
  }

  .field-controls input::placeholder {
    color: #6b7080;
  }

  .field-controls button {
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 12px;
    cursor: pointer;
  }

  .field-controls button:disabled {
    opacity: 0.4;
    cursor: default;
  }

  .field-controls button:not(:disabled):hover {
    background: var(--accent, #6ea8fe);
    color: #0c0e12;
    border-color: var(--accent, #6ea8fe);
  }

  .field-controls button.clear:not(:disabled):hover {
    background: #d9534f;
    border-color: #d9534f;
    color: #fff;
  }

  .field-error {
    margin: 6px 0 0;
    font-size: 12px;
    color: #d9534f;
  }

  /* FD7: reduced motion is a baseline accessibility default, not a setting. */
  @media (prefers-reduced-motion: reduce) {
    .chevron {
      transition: none;
    }
  }
</style>
