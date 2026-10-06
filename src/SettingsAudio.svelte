<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';

  // Mirrors `uia_app::config::AudioSection` (minus the fields this tab
  // doesn't expose) - duplicated across the IPC boundary like the other
  // settings tab's `Account` type.
  type AudioSettings = {
    input_device: string | null;
    output_device: string | null;
    aec_enabled: boolean;
    // `AudioSection::barge_in_threshold` is an `Option<f32>` and arrives as
    // null whenever neither the sidecar nor `uia.toml` has set one, so this
    // has to be nullable: typing it as a plain number let that null through
    // into `$state` unnoticed, and the label below died on `.toFixed()` of
    // null on every fresh install. `aec_enabled` needs no such treatment -
    // Rust resolves it with `unwrap_or` before it crosses the boundary.
    barge_in_threshold: number | null;
  };

  // uia-core's `Session::DEFAULT_BARGE_IN_THRESHOLD` - not `pub`, so this
  // mirrors the literal the way `uia.toml`'s own `[audio]` comment already
  // does, only as a display fallback for whenever `get_audio_settings`
  // reports no threshold has ever been set (fresh install, no override).
  const DEFAULT_BARGE_IN_THRESHOLD = 0.05;

  // The empty string is the "Default (recommended)" option's value - Rust's
  // `Option<String>` has no direct HTML <select> equivalent, so `null` maps
  // to `''` at this boundary and back on save.
  const DEFAULT_OPTION = '';

  let { onchange }: { onchange: () => void } = $props();

  let inputDevice = $state(DEFAULT_OPTION);
  let outputDevice = $state(DEFAULT_OPTION);
  let aecEnabled = $state(true);
  let bargeInThreshold = $state(DEFAULT_BARGE_IN_THRESHOLD);

  // Snapshot of what's actually persisted, refreshed on load and after every
  // successful save - compared against the live fields to grey the Save
  // button out until something has actually changed.
  let savedSettings = $state({
    inputDevice: DEFAULT_OPTION,
    outputDevice: DEFAULT_OPTION,
    aecEnabled: true,
    bargeInThreshold: DEFAULT_BARGE_IN_THRESHOLD,
  });
  const dirty = $derived(
    inputDevice !== savedSettings.inputDevice ||
      outputDevice !== savedSettings.outputDevice ||
      aecEnabled !== savedSettings.aecEnabled ||
      bargeInThreshold !== savedSettings.bargeInThreshold
  );

  let inputDeviceOptions = $state<string[]>([]);
  let outputDeviceOptions = $state<string[]>([]);

  let error = $state<string | null>(null);
  let saved = $state(false);

  // A device pinned in `uia.toml`/the sidecar might not exactly match any
  // name `list_*_devices` enumerates right now (matching is a
  // case-insensitive substring, see `uia.toml`'s `[audio]` comment) -
  // appending it rather than silently dropping it from the dropdown keeps the
  // picker honest about what's actually configured, even if unplugged.
  function withCurrentValue(options: string[], current: string | null): string[] {
    if (!current || options.includes(current)) return options;
    return [...options, current];
  }

  invoke<AudioSettings>('get_audio_settings')
    .then((settings) => {
      inputDevice = settings.input_device ?? DEFAULT_OPTION;
      outputDevice = settings.output_device ?? DEFAULT_OPTION;
      aecEnabled = settings.aec_enabled;
      bargeInThreshold = settings.barge_in_threshold ?? DEFAULT_BARGE_IN_THRESHOLD;
      inputDeviceOptions = withCurrentValue(inputDeviceOptions, settings.input_device);
      outputDeviceOptions = withCurrentValue(outputDeviceOptions, settings.output_device);
      savedSettings = { inputDevice, outputDevice, aecEnabled, bargeInThreshold };
    })
    .catch((e) => console.error('get_audio_settings invoke() failed:', e));

  invoke<string[]>('list_input_devices')
    .then((names) => {
      inputDeviceOptions = withCurrentValue(names, inputDevice || null);
    })
    .catch((e) => console.error('list_input_devices invoke() failed:', e));

  invoke<string[]>('list_output_devices')
    .then((names) => {
      outputDeviceOptions = withCurrentValue(names, outputDevice || null);
    })
    .catch((e) => console.error('list_output_devices invoke() failed:', e));

  async function save() {
    error = null;
    saved = false;
    try {
      await invoke('set_audio_settings', {
        inputDevice: inputDevice || null,
        outputDevice: outputDevice || null,
        aecEnabled,
        bargeInThreshold,
      });
      saved = true;
      savedSettings = { inputDevice, outputDevice, aecEnabled, bargeInThreshold };
      onchange();
    } catch (e) {
      error = String(e);
      console.error('set_audio_settings invoke() failed:', e);
    }
  }
</script>

<p class="intro">
  Windows can silently hold the wrong device under its own "Communications" role even when Sound
  settings shows the right one as default - pin the exact device here to bypass that entirely.
</p>

<section class="field">
  <label for="input_device">Input device</label>
  <select id="input_device" bind:value={inputDevice}>
    <option value={DEFAULT_OPTION}>Default (recommended)</option>
    {#each inputDeviceOptions as name (name)}
      <option value={name}>{name}</option>
    {/each}
  </select>
</section>

<section class="field">
  <label for="output_device">Output device</label>
  <select id="output_device" bind:value={outputDevice}>
    <option value={DEFAULT_OPTION}>Default (recommended)</option>
    {#each outputDeviceOptions as name (name)}
      <option value={name}>{name}</option>
    {/each}
  </select>
</section>

<section class="field">
  <label class="checkbox-label">
    <input type="checkbox" bind:checked={aecEnabled} />
    Echo cancellation (AEC)
  </label>
  <p class="hint">
    Applies Windows' own noise/echo cancellation to the input and output devices above. Turn off
    only to diagnose audio issues - without it the assistant will hear itself through speakers.
  </p>
</section>

<section class="field">
  <label for="barge_in_threshold">
    Barge-in sensitivity ({bargeInThreshold.toFixed(2)})
  </label>
  <input
    id="barge_in_threshold"
    type="range"
    min="0"
    max="0.5"
    step="0.01"
    bind:value={bargeInThreshold}
  />
  <p class="hint">
    RMS level above which speaking over the assistant counts as a barge-in. Lower if interrupting
    it feels laggy; raise if background noise triggers false barge-ins.
  </p>
</section>

<div class="save-row">
  <button type="button" disabled={!dirty} onclick={save}>Save</button>
  {#if saved}
    <span class="saved-notice">Saved.</span>
  {/if}
</div>
{#if error}
  <p class="field-error">{error}</p>
{/if}

<style>
  .intro {
    margin: 0 0 24px;
    color: #b8bcc6;
    font-size: 13px;
  }

  .field {
    margin-bottom: 20px;
  }

  .field label {
    display: block;
    font-weight: 500;
    margin-bottom: 6px;
  }

  .field select,
  .field input[type='range'] {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 10px;
  }

  .field input[type='range'] {
    padding: 0;
    accent-color: var(--accent, #6ea8fe);
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

  .save-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .save-row button {
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 12px;
    cursor: pointer;
  }

  .save-row button:disabled {
    opacity: 0.4;
    cursor: default;
  }

  .save-row button:not(:disabled):hover {
    background: var(--accent, #6ea8fe);
    color: #0c0e12;
    border-color: var(--accent, #6ea8fe);
  }

  .saved-notice {
    font-size: 12px;
    color: var(--accent, #6ea8fe);
  }

  .field-error {
    margin: 10px 0 0;
    font-size: 12px;
    color: #d9534f;
  }
</style>
