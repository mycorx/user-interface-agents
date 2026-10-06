<script lang="ts" module>
  // Mirrors `uia_app::settings::UiSettings` - duplicated across the IPC
  // boundary like the other settings tabs' types. `theme` of `null` means
  // "follow prefers-color-scheme"; CardDeck.svelte resolves that fallback
  // since it is the one place with the browser-side query. `glass_tint`
  // (S11) is `null`/`"rim"` for the accent border/edge-light only, or
  // `"wash"` to also blend the accent into the glass fill.
  export type UiSettings = { theme: string | null; accent: string | null; glass_tint: string | null };
</script>

<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { untrack } from 'svelte';

  // The prototype's exact four values (PLAN.md's stage list for this file).
  const ACCENT_SWATCHES = ['#6ea8fe', '#c9a24a', '#8b7fd6', '#5fd0c0'];
  const DEFAULT_ACCENT = ACCENT_SWATCHES[0];

  let {
    uiSettings,
    onchange,
  }: {
    uiSettings: UiSettings;
    onchange: (settings: UiSettings) => void;
  } = $props();

  function resolveTheme(theme: string | null): 'dark' | 'light' {
    if (theme === 'light' || theme === 'dark') return theme;
    return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  }

  // Captured once at mount, not tracked reactively against `uiSettings` -
  // CardDeck already applied this value live before Settings could ever be
  // opened, so re-deriving on every parent update would just fight the
  // user's own in-progress edits below.
  let theme = $state<'dark' | 'light'>(untrack(() => resolveTheme(uiSettings.theme)));
  let accent = $state(untrack(() => uiSettings.accent ?? DEFAULT_ACCENT));
  let glassTint = $state<'rim' | 'wash'>(
    untrack(() => (uiSettings.glass_tint === 'wash' ? 'wash' : 'rim'))
  );

  // Snapshot of what's actually persisted to `uia-ui.json`, taken once at
  // mount from the same values `theme`/`accent`/`glassTint` start from (this
  // component's own prop, before any live edit below diverges from it) and
  // refreshed after every successful save - `dirty` is what SettingsCard's
  // footer Save button greys itself against.
  let savedTheme = $state(untrack(() => theme));
  let savedAccent = $state(untrack(() => accent));
  let savedGlassTint = $state(untrack(() => glassTint));
  let dirty = $derived(
    theme !== savedTheme || accent !== savedAccent || glassTint !== savedGlassTint
  );

  // A `$derived` can't be exported directly (Svelte requires a function),
  // hence the accessor - the parent's `disabled` expression still tracks it
  // reactively since the read happens inside that same render pass.
  export function isDirty(): boolean {
    return dirty;
  }

  function setTheme(next: 'dark' | 'light') {
    theme = next;
    onchange({ theme, accent, glass_tint: glassTint });
  }

  function setAccent(next: string) {
    accent = next;
    onchange({ theme, accent, glass_tint: glassTint });
  }

  function setGlassTint(next: 'rim' | 'wash') {
    glassTint = next;
    onchange({ theme, accent, glass_tint: glassTint });
  }

  // Called by SettingsCard's footer Save button via `bind:this` - S9 owns no
  // new component boundary beyond this file, per S6's handoff note.
  export async function save(): Promise<void> {
    await invoke('set_ui_settings', { settings: { theme, accent, glass_tint: glassTint } });
    savedTheme = theme;
    savedAccent = accent;
    savedGlassTint = glassTint;
  }
</script>

<section class="field">
  <span class="field-label">Theme</span>
  <div class="theme-switch" role="radiogroup" aria-label="Theme">
    <button
      type="button"
      role="radio"
      aria-checked={theme === 'dark'}
      class:active={theme === 'dark'}
      onclick={() => setTheme('dark')}
    >
      Dark
    </button>
    <button
      type="button"
      role="radio"
      aria-checked={theme === 'light'}
      class:active={theme === 'light'}
      onclick={() => setTheme('light')}
    >
      Light
    </button>
  </div>
</section>

<section class="field">
  <span class="field-label">Accent color</span>
  <div class="swatches" role="radiogroup" aria-label="Accent color">
    {#each ACCENT_SWATCHES as swatch (swatch)}
      <button
        type="button"
        role="radio"
        aria-checked={accent === swatch}
        aria-label={swatch}
        class="swatch"
        class:active={accent === swatch}
        style="--swatch: {swatch}"
        onclick={() => setAccent(swatch)}
      ></button>
    {/each}
  </div>
</section>

<section class="field">
  <span class="field-label">Accent glow</span>
  <div class="theme-switch" role="radiogroup" aria-label="Accent glow">
    <button
      type="button"
      role="radio"
      aria-checked={glassTint === 'rim'}
      class:active={glassTint === 'rim'}
      onclick={() => setGlassTint('rim')}
    >
      Rim
    </button>
    <button
      type="button"
      role="radio"
      aria-checked={glassTint === 'wash'}
      class:active={glassTint === 'wash'}
      onclick={() => setGlassTint('wash')}
    >
      Full wash
    </button>
  </div>
</section>

<style>
  .field {
    margin-bottom: 24px;
  }

  .field-label {
    display: block;
    font-weight: 500;
    margin-bottom: 8px;
  }

  .theme-switch {
    display: inline-flex;
    gap: 4px;
    padding: 3px;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
  }

  .theme-switch button {
    font: inherit;
    color: #8890a0;
    background: none;
    border: none;
    border-radius: 6px;
    padding: 6px 14px;
    cursor: pointer;
  }

  .theme-switch button:hover {
    color: #e8eaf0;
  }

  .theme-switch button.active {
    color: #0c0e12;
    background: var(--accent, #6ea8fe);
  }

  .swatches {
    display: flex;
    gap: 12px;
  }

  .swatch {
    width: 28px;
    height: 28px;
    padding: 0;
    border-radius: 50%;
    background: var(--swatch);
    border: 2px solid transparent;
    cursor: pointer;
  }

  .swatch:hover {
    border-color: rgba(255, 255, 255, 0.4);
  }

  .swatch.active {
    border-color: #e8eaf0;
    box-shadow: 0 0 0 2px var(--swatch);
  }
</style>
