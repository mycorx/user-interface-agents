<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow, LogicalSize } from '@tauri-apps/api/window';
  import { WebviewWindow } from '@tauri-apps/api/webviewWindow';
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { tick } from 'svelte';

  // The floating MCP alert stack: up to three cards, oldest first. The backend
  // owns the list; this page re-reads it on every event and after every
  // dismiss instead of keeping a list of its own. Every server-provided string
  // is rendered with text interpolation only, never as HTML.

  type Alert = {
    card_id: number;
    server: string;
    title: string | null;
    summary: string;
    details: string | null;
    action: { label: string; url: string; host: string } | null;
  };

  const WINDOW_WIDTH = 380;
  const AUTO_DISMISS_MS = 30_000;
  const DEFAULT_ACCENT = '#6ea8fe';
  const POINTER_POLL_MS = 100;

  let alerts = $state<Alert[]>([]);
  let stack: HTMLDivElement | undefined = $state();
  const timers = new Map<number, ReturnType<typeof setTimeout>>();
  const hovered = new Set<number>();
  let accent = $state(DEFAULT_ACCENT);
  let lightMode = $state(false);
  let glassWash = $state(false);

  // This window never takes focus, and WebKit on macOS only tracks the mouse
  // in the key window, so neither `:hover` nor `mouseenter` fires there until
  // a click. Asking the backend where the cursor is gives the same answer on
  // every platform. The native events stay for a platform that cannot report
  // the cursor (Wayland), where polling stops at its first failure.
  let pointedButton = $state<string | null>(null);
  let pointedCard: number | null = null;
  let pollFailed = false;
  let polling = false;

  async function pollPointer() {
    if (polling || pollFailed) return;
    polling = true;
    try {
      const [x, y] = await invoke<[number, number]>('alert_cursor_position');
      const under = document.elementFromPoint(x, y);
      const card = under?.closest<HTMLElement>('[data-card-id]');
      const cardId = card ? Number(card.dataset.cardId) : null;
      if (cardId !== pointedCard) {
        if (pointedCard !== null) hoverEnd(pointedCard);
        if (cardId !== null) hoverStart(cardId);
        pointedCard = cardId;
      }
      pointedButton = under?.closest<HTMLElement>('[data-hover-id]')?.dataset.hoverId ?? null;
    } catch (e) {
      pollFailed = true;
      console.error('alert_cursor_position invoke() failed:', e);
    } finally {
      polling = false;
    }
  }

  const anyCards = $derived(alerts.length > 0);

  $effect(() => {
    if (!anyCards) return;
    const poll = setInterval(() => void pollPointer(), POINTER_POLL_MS);
    return () => {
      clearInterval(poll);
      pointedButton = null;
      pointedCard = null;
    };
  });

  // The look chosen in Settings, resolved the way CardDeck.svelte resolves it
  // (a null theme follows the OS). Read on every alert rather than once: this
  // window outlives a change in Settings, and an alert is the only time it is
  // seen.
  async function loadLook() {
    try {
      const settings = await invoke<{
        theme: string | null;
        accent: string | null;
        glass_tint: string | null;
      }>('get_ui_settings');
      accent = settings.accent ?? DEFAULT_ACCENT;
      glassWash = settings.glass_tint === 'wash';
      lightMode =
        settings.theme === 'light' ||
        (settings.theme !== 'dark' && window.matchMedia('(prefers-color-scheme: light)').matches);
    } catch (e) {
      console.error('get_ui_settings invoke() failed:', e);
    }
  }

  async function refresh() {
    await loadLook();
    try {
      alerts = await invoke<Alert[]>('take_pending_alerts');
    } catch (e) {
      console.error('take_pending_alerts invoke() failed:', e);
      return;
    }
    syncTimers();
    await fitWindow();
  }

  async function fitWindow() {
    await tick();
    if (!stack || alerts.length === 0) return;
    const height = Math.ceil(stack.getBoundingClientRect().height);
    getCurrentWindow()
      .setSize(new LogicalSize(WINDOW_WIDTH, height))
      .catch((e) => console.error('alert window setSize() failed:', e));
  }

  // One 30 s timer per live card; a card that is gone loses its timer.
  function syncTimers() {
    const live = new Set(alerts.map((a) => a.card_id));
    for (const id of [...timers.keys()]) {
      if (!live.has(id)) stopTimer(id);
    }
    for (const id of [...hovered]) {
      if (!live.has(id)) hovered.delete(id);
    }
    for (const id of live) {
      if (!timers.has(id) && !hovered.has(id)) startTimer(id);
    }
  }

  function startTimer(id: number) {
    timers.set(
      id,
      setTimeout(() => void dismiss(id), AUTO_DISMISS_MS)
    );
  }

  function stopTimer(id: number) {
    const t = timers.get(id);
    if (t !== undefined) clearTimeout(t);
    timers.delete(id);
  }

  // Hover pauses the countdown; leaving restarts the full 30 s.
  function hoverStart(id: number) {
    hovered.add(id);
    stopTimer(id);
  }

  function hoverEnd(id: number) {
    hovered.delete(id);
    if (alerts.some((a) => a.card_id === id) && !timers.has(id)) startTimer(id);
  }

  async function dismiss(cardId: number) {
    stopTimer(cardId);
    try {
      await invoke('dismiss_alert', { cardId });
    } catch (e) {
      console.error('dismiss_alert invoke() failed:', e);
    }
    await refresh();
  }

  async function showHud() {
    try {
      const hud = await WebviewWindow.getByLabel('hud');
      if (!hud) return;
      await hud.unminimize();
      await hud.show();
      await hud.setFocus();
    } catch (e) {
      console.error('showing the hud window failed:', e);
    }
  }

  async function runAction(alert: Alert) {
    if (!alert.action) return;
    try {
      await openUrl(alert.action.url);
    } catch (e) {
      // Keep the card so the user can retry or dismiss it.
      console.error('openUrl() failed:', e);
      return;
    }
    await dismiss(alert.card_id);
  }

  void refresh();

  listen('uia://mcp-alert', () => {
    void refresh();
  }).catch((e) => console.error('uia://mcp-alert listen() failed:', e));
</script>

<span id="open-hint" class="sr-only">Opens the assistant</span>
<div
  class="stack"
  class:light-mode={lightMode}
  class:glass-wash={glassWash}
  style="--accent: {accent}"
  bind:this={stack}
>
  {#each alerts as alert (alert.card_id)}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="card"
      data-card-id={alert.card_id}
      role="status"
      aria-live="polite"
      onmouseenter={() => hoverStart(alert.card_id)}
      onmouseleave={() => hoverEnd(alert.card_id)}
    >
      <button
        type="button"
        class="body"
        aria-describedby="open-hint"
        onclick={showHud}
      >
        <span class="server">{alert.server}</span>
        {#if alert.title}<span class="title">{alert.title}</span>{/if}
        <span class="summary">{alert.summary}</span>
        {#if alert.details}<span class="details">{alert.details}</span>{/if}
      </button>
      <div class="actions">
        {#if alert.action}
          <button
            type="button"
            class="primary"
            class:hover={pointedButton === `${alert.card_id}-action`}
            data-hover-id="{alert.card_id}-action"
            onclick={() => runAction(alert)}
          >
            {alert.action.label}
            <span class="host">· {alert.action.host}</span>
          </button>
        {/if}
        <button
          type="button"
          class="dismiss"
          class:hover={pointedButton === `${alert.card_id}-dismiss`}
          data-hover-id="{alert.card_id}-dismiss"
          onclick={() => dismiss(alert.card_id)}
        >
          Dismiss
        </button>
      </div>
    </div>
  {/each}
</div>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    background: transparent;
    overflow: hidden;
    font-family:
      system-ui,
      -apple-system,
      'Segoe UI',
      sans-serif;
    font-size: 13px;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }

  .stack {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 8px;
    box-sizing: border-box;
    width: 380px;

    /* The main client's tokens (CardDeck.svelte). The fill is denser than its
       glass because this window has no native acrylic behind it, and the card
       has to stay readable over whatever is on screen. */
    --tint: 255, 255, 255;
    --ink-1: #e8eaf0;
    --ink-4: #8890a0;
    --card-fill: rgba(18, 20, 26, 0.92);
    color: var(--ink-1);
  }

  .stack.light-mode {
    --tint: 20, 22, 30;
    --ink-1: #1b1d24;
    --ink-4: #6b7080;
    --card-fill: rgba(255, 255, 255, 0.94);
  }

  .card {
    background: var(--card-fill);
    border: 1px solid rgba(var(--tint), 0.12);
    border-radius: 14px;
    padding: 12px 14px;
    /* The accent rim the HUD card wears, so the two read as one app. */
    box-shadow:
      0 6px 20px rgba(0, 0, 0, 0.35),
      0 0 0 1px color-mix(in srgb, var(--accent) 30%, transparent) inset,
      0 0 16px -3px color-mix(in srgb, var(--accent) 50%, transparent) inset;
    animation: appear 160ms ease-out;
  }

  .glass-wash .card {
    background-image: linear-gradient(
      160deg,
      color-mix(in srgb, var(--accent) 22%, transparent),
      transparent 65%
    );
  }

  @media (prefers-reduced-motion: reduce) {
    .card {
      animation: none;
    }
  }

  @keyframes appear {
    from {
      opacity: 0;
      transform: translateY(-6px);
    }
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: 3px;
    width: 100%;
    font: inherit;
    color: inherit;
    text-align: left;
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
  }

  .server {
    font-size: 11px;
    color: var(--ink-4);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .title {
    font-weight: 600;
  }

  .summary,
  .title,
  .details {
    overflow-wrap: anywhere;
  }

  .details {
    color: var(--ink-4);
  }

  .actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    margin-top: 10px;
  }

  .actions button {
    font: inherit;
    border-radius: 8px;
    padding: 5px 12px;
    cursor: pointer;
    border: 1px solid rgba(var(--tint), 0.12);
    background: rgba(var(--tint), 0.06);
    color: var(--ink-1);
  }

  .actions button:hover,
  .actions button.hover {
    background: rgba(var(--tint), 0.16);
    border-color: color-mix(in srgb, var(--accent) 60%, transparent);
  }

  .actions .primary {
    color: #0c0e12;
    background: var(--accent);
    border-color: transparent;
  }

  .actions .primary:hover,
  .actions .primary.hover {
    background: color-mix(in srgb, var(--accent) 80%, white);
    border-color: transparent;
  }

  .host {
    opacity: 0.75;
  }

  button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
</style>
