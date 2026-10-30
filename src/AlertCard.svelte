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

  let alerts = $state<Alert[]>([]);
  let stack: HTMLDivElement | undefined = $state();
  const timers = new Map<number, ReturnType<typeof setTimeout>>();
  const hovered = new Set<number>();

  async function refresh() {
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
      console.error('openUrl() failed:', e);
    }
  }

  void refresh();

  listen('uia://mcp-alert', () => {
    void refresh();
  }).catch((e) => console.error('uia://mcp-alert listen() failed:', e));
</script>

<div class="stack" bind:this={stack}>
  {#each alerts as alert (alert.card_id)}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="card"
      role="status"
      aria-live="polite"
      onmouseenter={() => hoverStart(alert.card_id)}
      onmouseleave={() => hoverEnd(alert.card_id)}
    >
      <button
        type="button"
        class="body"
        aria-label={`Open the assistant: ${alert.title ?? alert.server}`}
        onclick={showHud}
      >
        <span class="server">{alert.server}</span>
        {#if alert.title}<span class="title">{alert.title}</span>{/if}
        <span class="summary">{alert.summary}</span>
        {#if alert.details}<span class="details">{alert.details}</span>{/if}
      </button>
      <div class="actions">
        {#if alert.action}
          <button type="button" class="primary" onclick={() => runAction(alert)}>
            {alert.action.label}
            <span class="host">· {alert.action.host}</span>
          </button>
        {/if}
        <button type="button" class="dismiss" onclick={() => dismiss(alert.card_id)}>
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
    color: #e8eaf0;
  }

  .stack {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 8px;
    box-sizing: border-box;
    width: 380px;
  }

  .card {
    background: rgba(18, 20, 26, 0.92);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 14px;
    padding: 12px 14px;
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.35);
    animation: appear 160ms ease-out;
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
    color: #8890a0;
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
    color: #8890a0;
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
    border: 1px solid rgba(255, 255, 255, 0.12);
    background: rgba(255, 255, 255, 0.06);
    color: #e8eaf0;
  }

  .actions button:hover {
    background: rgba(255, 255, 255, 0.12);
  }

  .actions .primary {
    color: #0c0e12;
    background: #6ea8fe;
    border-color: transparent;
  }

  .actions .primary:hover {
    background: #8bb9ff;
  }

  .host {
    opacity: 0.75;
  }

  button:focus-visible {
    outline: 2px solid #6ea8fe;
    outline-offset: 2px;
  }
</style>
