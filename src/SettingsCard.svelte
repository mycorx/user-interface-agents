<script lang="ts">
  import { relaunch } from '@tauri-apps/plugin-process';
  import SettingsProviders from './SettingsProviders.svelte';
  import SettingsAudio from './SettingsAudio.svelte';
  import SettingsGeneral from './SettingsGeneral.svelte';
  import SettingsVisual, { type UiSettings } from './SettingsVisual.svelte';
  import SettingsMcp from './SettingsMcp.svelte';

  // Replaces Settings.svelte inside the card deck. Per FD13(b) this defines
  // the full four-tab set now, with placeholder panels for Visual and
  // General, so S8/S9/S10 each own exactly one file and none of them
  // touches the tab list. Per S1's follow-up notes, the header and tab bar
  // are pinned outside the scroll region (`.panel-head`/`.panel-scroll`)
  // instead of scrolling away with `Settings.svelte`'s old single-`<main>`
  // layout.
  type Tab = 'providers' | 'audio' | 'visual' | 'general' | 'mcp';
  // S7 re-ordered the tabs to General / Visual / Audio / Providers and moved
  // the default with them: opening on Providers left the far-right tab
  // underlined while the eye started at the left. S6's "credentials are the
  // reason this window exists" reasoning held while Providers led the row;
  // it does not survive the re-order.
  //
  // The row is data rather than five hand-written buttons because the
  // travelling highlight below has to measure the active tab by index.
  const TABS: readonly { id: Tab; label: string }[] = [
    { id: 'general', label: 'General' },
    { id: 'visual', label: 'Visual' },
    { id: 'audio', label: 'Audio' },
    { id: 'providers', label: 'Providers' },
    { id: 'mcp', label: 'MCP' },
  ];
  let activeTab = $state<Tab>('general');

  // --- travelling tab highlight ------------------------------------------
  // One shared bar rather than five border-bottoms that flip on and off: the
  // eye can follow the selection across the row instead of re-finding it.
  // The liquid read comes entirely from timing the two edges differently -
  // the leading edge leaves at LEAD_MS while the trailing edge lags to
  // TRAIL_MS, so the bar elongates in flight and contracts as it settles.
  const LEAD_MS = 280;
  const TRAIL_MS = 440;

  let tabsEl = $state<HTMLDivElement>();
  let tabEls = $state<HTMLButtonElement[]>([]);
  let indicatorEl = $state<HTMLSpanElement>();
  // The first successful placement is silent. Without it the bar flies in
  // from the left edge of the row every time Settings opens.
  let placed = false;
  let prevIndex = -1;

  function placeIndicator(animate: boolean): boolean {
    const bar = tabsEl;
    const ind = indicatorEl;
    const index = TABS.findIndex((t) => t.id === activeTab);
    const btn = tabEls[index];
    if (!bar || !ind || !btn) return false;

    // The card can be measured before it has been laid out, where every
    // offset reads 0. Placing on those numbers pins the bar to left:0
    // right:0 - a highlight stretched across the entire row the moment real
    // layout arrives, and `placed` would then suppress any correction. Bail
    // and let the ResizeObserver below place it once the row has a width.
    const barWidth = bar.clientWidth;
    if (barWidth === 0 || btn.offsetWidth === 0) return false;

    const left = btn.offsetLeft;
    const right = barWidth - (btn.offsetLeft + btn.offsetWidth);

    if (animate) {
      // Whichever edge faces the destination is the one that leads.
      const forward = index > prevIndex;
      ind.style.setProperty('--dur-left', `${forward ? TRAIL_MS : LEAD_MS}ms`);
      ind.style.setProperty('--dur-right', `${forward ? LEAD_MS : TRAIL_MS}ms`);
      ind.style.removeProperty('transition');
      // Toggled imperatively (and matched with :global below) because a
      // reactive class cannot restart a keyframe: Svelte batches the off and
      // the on into a single update, so the browser never sees it stop.
      ind.classList.remove('flying');
      void ind.offsetWidth;
      ind.classList.add('flying');
    } else {
      ind.classList.remove('flying');
      ind.style.transition = 'none';
    }

    ind.style.left = `${left}px`;
    ind.style.right = `${right}px`;

    if (!animate) {
      void ind.offsetWidth;
      ind.style.removeProperty('transition');
    }

    prevIndex = index;
    return true;
  }

  $effect(() => {
    if (placeIndicator(placed)) placed = true;
  });

  $effect(() => {
    const bar = tabsEl;
    if (!bar) return;
    // The row's first real width is what the initial placement is waiting
    // for, and this is the only reliable signal that it has one. It also
    // covers a late-loading face changing label widths afterwards, which
    // would otherwise leave the bar a couple of pixels off its tab.
    const observer = new ResizeObserver(() => {
      if (placeIndicator(false)) placed = true;
    });
    observer.observe(bar);
    return () => observer.disconnect();
  });

  // A single page-level notice rather than one per tab: nothing Providers or
  // Audio persist affects a running session live - it was already
  // constructed with whatever resolved at connect time - so one restart
  // covers every change made during this visit, across both tabs.
  let restartNoticeVisible = $state(false);

  // FD5: Visual applies live with no restart, so it does not use the notice
  // above - it gets its own Save/Saved-flash footer instead. The footer bar
  // is always-mounted chrome (so switching tabs never shifts the panel's
  // height); only the Visual tab currently drives it. S9 replaces the
  // no-op handler below with the real save when it plugs in `SettingsVisual`
  // (its own stage's Artifacts note it edits this file directly).
  let visualSaveState = $state<'idle' | 'saving' | 'saved'>('idle');
  let visualCard: SettingsVisual | undefined = $state();

  async function saveVisual() {
    visualSaveState = 'saving';
    try {
      await visualCard?.save();
      visualSaveState = 'saved';
      setTimeout(() => {
        visualSaveState = 'idle';
      }, 1500);
    } catch (e) {
      console.error('Visual save failed:', e);
      visualSaveState = 'idle';
    }
  }

  let {
    onclose,
    uiSettings,
    onvisualchange,
  }: {
    onclose: () => void;
    uiSettings: UiSettings;
    onvisualchange: (settings: UiSettings) => void;
  } = $props();

  async function restartNow() {
    try {
      await relaunch();
    } catch (e) {
      console.error('relaunch() failed:', e);
    }
  }
</script>

<div class="panel-head">
  <!-- Draggable like the HUD's own top row: the window is undecorated, so
       without a drag region the settings card is a window the user cannot
       move. The X button keeps working because Tauri only treats the
       element carrying the attribute as the handle, not its children. -->
  <div class="header" data-tauri-drag-region>
    <h1 data-tauri-drag-region>Settings</h1>
    <button type="button" class="close" onclick={onclose} aria-label="Back to HUD">
      <svg class="close-glyph" viewBox="0 0 24 24" aria-hidden="true">
        <line x1="6" y1="6" x2="18" y2="18" />
        <line x1="18" y1="6" x2="6" y2="18" />
      </svg>
    </button>
  </div>

  <div class="tabs" role="tablist" bind:this={tabsEl}>
    {#each TABS as tab, i (tab.id)}
      <button
        type="button"
        role="tab"
        aria-selected={activeTab === tab.id}
        class:active={activeTab === tab.id}
        onclick={() => (activeTab = tab.id)}
        bind:this={tabEls[i]}
      >
        {tab.label}
      </button>
    {/each}
    <span class="tab-indicator" bind:this={indicatorEl} aria-hidden="true"></span>
  </div>
</div>

<div class="panel-scroll">
  <div class="tab-panel">
    {#if activeTab === 'providers'}
      <SettingsProviders onchange={() => (restartNoticeVisible = true)} />
    {:else if activeTab === 'audio'}
      <SettingsAudio onchange={() => (restartNoticeVisible = true)} />
    {:else if activeTab === 'visual'}
      <SettingsVisual bind:this={visualCard} {uiSettings} onchange={onvisualchange} />
    {:else if activeTab === 'mcp'}
      <SettingsMcp onchange={() => (restartNoticeVisible = true)} />
    {:else}
      <SettingsGeneral onchange={() => (restartNoticeVisible = true)} />
    {/if}
  </div>

  {#if restartNoticeVisible}
    <p class="restart-notice">
      Restart the assistant for these changes to take effect.
      <button type="button" class="restart-now" onclick={restartNow}>Restart Now</button>
    </p>
  {/if}
</div>

<div class="panel-footer">
  {#if activeTab === 'visual'}
    <button
      type="button"
      onclick={saveVisual}
      disabled={visualSaveState === 'saving' || !visualCard?.isDirty()}
    >
      {visualSaveState === 'saving' ? 'Saving…' : 'Save'}
    </button>
    {#if visualSaveState === 'saved'}
      <span class="saved-flash">Saved.</span>
    {/if}
  {/if}
</div>

<style>
  .panel-head {
    flex-shrink: 0;
    box-sizing: border-box;
    padding: 22px 24px 0;
    color: var(--ink-1, #e8eaf0);
    font: 400 14px/1.5 system-ui, sans-serif;
  }

  h1 {
    margin: 0;
    font-size: 18px;
    font-weight: 600;
  }

  .header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 16px;
  }

  .close {
    font: inherit;
    color: #8890a0;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    width: 28px;
    height: 28px;
    /* Default <button> side padding would push the icon off-centre. */
    padding: 0;
    display: grid;
    place-items: center;
    line-height: 1;
    cursor: pointer;
  }

  .close-glyph {
    width: 16px;
    height: 16px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
  }

  .close:hover {
    color: #e8eaf0;
  }

  .tabs {
    /* Containing block for the travelling highlight. */
    position: relative;
    display: flex;
    gap: 4px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }

  .tabs button {
    /* Above the indicator, and the transparent border keeps the row exactly
       the height it was when each button drew its own underline. */
    position: relative;
    z-index: 1;
    font: inherit;
    color: #8890a0;
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    padding: 8px 4px;
    margin-right: 16px;
    cursor: pointer;
    transition: color 180ms ease;
  }

  .tabs button:hover {
    color: #e8eaf0;
  }

  .tabs button.active {
    color: #e8eaf0;
  }

  /* The highlight is one shared bar that travels, not five borders that
     flip. It is positioned by `left`/`right` rather than `left`/`width`
     precisely so the two edges can carry their own durations: the leading
     edge leaves first, the trailing edge lags, and the bar stretches in
     flight and contracts as it settles. A `transform` + `scaleX` version
     cannot do that without smearing the 2px `border-radius`. `bottom: 0`
     lands it exactly where the per-button border used to sit, on top of the
     row's own hairline. */
  .tab-indicator {
    position: absolute;
    bottom: 0;
    height: 2px;
    border-radius: 2px;
    background: var(--accent, #6ea8fe);
    pointer-events: none;
    transition:
      left var(--dur-left, 280ms) cubic-bezier(0.19, 1, 0.22, 1),
      right var(--dur-right, 280ms) cubic-bezier(0.19, 1, 0.22, 1);
  }

  /* The bloom rides on `filter` so the bar and the glow it casts brighten in
     a single pass, and multiplicatively - which is why it needs no per-accent
     tuning and still reads once --accent is not blue. `:global` because the
     class is applied imperatively (see `placeIndicator`), so the compiler
     cannot see it in the markup and would otherwise prune this rule. */
  .tab-indicator:global(.flying) {
    animation: tabFlow 440ms ease-out;
  }

  @keyframes tabFlow {
    0% {
      filter: brightness(1);
      box-shadow: 0 0 0 0 transparent;
    }
    40% {
      filter: brightness(1.5);
      box-shadow: 0 0 11px -1px color-mix(in srgb, var(--accent, #6ea8fe) 75%, transparent);
    }
    100% {
      filter: brightness(1);
      box-shadow: 0 0 0 0 transparent;
    }
  }

  /* FD7: reduced motion is a baseline accessibility default, not a setting.
     The bar still lands on the right tab - it just stops travelling. */
  @media (prefers-reduced-motion: reduce) {
    .tabs button,
    .tab-indicator,
    .tab-indicator:global(.flying) {
      transition: none !important;
      animation: none !important;
    }
  }

  .panel-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    box-sizing: border-box;
    padding: 20px 24px 18px;
    color: var(--ink-1, #e8eaf0);
    font: 400 14px/1.5 system-ui, sans-serif;
  }

  /* The scrollbar sat unstyled and square across the card's rounded corners
     before S6 - keep it slim and glassy instead of the native default. */
  .panel-scroll::-webkit-scrollbar {
    width: 8px;
  }

  .panel-scroll::-webkit-scrollbar-track {
    background: transparent;
  }

  .panel-scroll::-webkit-scrollbar-thumb {
    background: rgba(255, 255, 255, 0.16);
    border-radius: 8px;
  }

  .panel-scroll::-webkit-scrollbar-thumb:hover {
    background: rgba(255, 255, 255, 0.28);
  }

  .restart-notice {
    margin: 20px 0 0;
    font-size: 13px;
    color: #b8bcc6;
  }

  .restart-now {
    font: inherit;
    color: var(--accent, #6ea8fe);
    background: none;
    border: none;
    padding: 0;
    margin-left: 6px;
    text-decoration: underline;
    cursor: pointer;
  }

  .panel-footer {
    flex-shrink: 0;
    box-sizing: border-box;
    min-height: 20px;
    padding: 0 24px 18px;
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .panel-footer button {
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 12px;
    cursor: pointer;
  }

  .panel-footer button:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .panel-footer button:not(:disabled):hover {
    background: var(--accent, #6ea8fe);
    color: #0c0e12;
    border-color: var(--accent, #6ea8fe);
  }

  .saved-flash {
    font-size: 12px;
    color: var(--accent, #6ea8fe);
  }
</style>
