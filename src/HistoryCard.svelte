<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';

  // The stored conversation, read back from `uia-conversation.jsonl`.
  //
  // A snapshot rather than a live feed, deliberately (the design doc's first
  // non-goal). Turns reach the store one completed exchange at a time, so
  // there is nothing to stream that would not also need a partial-exchange
  // state to render — and the store on disk is the only source of truth for
  // what was actually kept. Reopening the card, or the refresh button, is how
  // it advances.

  let { onclose }: { onclose: () => void } = $props();

  // Mirrors `get_history`'s payload in `main.rs`. Either half may be absent:
  // everything recorded before the session began asking engines to transcribe
  // the user has an assistant half and nothing else, and nothing can backfill
  // it. Rendered as exactly that rather than hidden or given a placeholder.
  type Turn = { user: string | null; assistant: string | null };
  type History = { enabled: boolean; turns: Turn[] };

  // Matches `RETAIN_LIMIT` in `crates/uia-app/src/memory.rs`. Stated in the
  // footer rather than silently enforced, so a user who scrolls to the top and
  // finds their first conversation missing knows why.
  const RETAIN_LIMIT = 200;

  let history = $state<History | null>(null);
  let failed = $state('');
  let loading = $state(true);
  let scroller = $state<HTMLDivElement | null>(null);

  async function load(scroll: boolean) {
    loading = true;
    try {
      history = await invoke<History>('get_history');
      failed = '';
      if (scroll) queueMicrotask(scrollToBottom);
    } catch (e) {
      console.error('get_history invoke() failed:', e);
      failed = String(e);
    } finally {
      loading = false;
    }
  }

  // Newest at the bottom, so the card opens where a reader wants to start —
  // on the most recent exchange, the way a chat log does.
  function scrollToBottom() {
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  }

  void load(true);

  const turns = $derived(history?.turns ?? []);
  const enabled = $derived(history?.enabled ?? false);
</script>

<div class="history">
  <header class="history-head">
    <h2>Conversation</h2>
    <span class="spacer"></span>
    <button
      type="button"
      class="chip"
      aria-label="Refresh"
      title="Refresh"
      disabled={loading}
      onclick={() => load(true)}
    >
      <svg class="chip-glyph" viewBox="0 0 24 24" aria-hidden="true">
        <path d="M20 12a8 8 0 1 1-2.3-5.6L20 8.5" />
        <polyline points="20 3.5 20 8.5 15 8.5" />
      </svg>
    </button>
    <button type="button" class="chip" aria-label="Close" title="Close" onclick={onclose}>
      <svg class="chip-glyph" viewBox="0 0 24 24" aria-hidden="true">
        <line x1="6" y1="6" x2="18" y2="18" />
        <line x1="18" y1="6" x2="6" y2="18" />
      </svg>
    </button>
  </header>

  <div class="history-scroll" bind:this={scroller}>
    {#if failed}
      <p class="history-empty error">Could not read the stored conversation: {failed}</p>
    {:else if loading && !history}
      <p class="history-empty">Reading the conversation…</p>
    {:else if !enabled}
      <!-- The two empty histories look identical in `turns` and mean opposite
           things, which is why `get_history` reports `enabled` separately. -->
      <p class="history-empty">
        Nothing is being recorded. Turn on <strong>Remember the conversation</strong> in Settings →
        General to keep a transcript.
      </p>
    {:else if turns.length === 0}
      <p class="history-empty">No exchanges recorded yet. Say something and it will appear here.</p>
    {:else}
      {#each turns as turn, i (i)}
        <div class="turn">
          {#if turn.user}
            <p class="line user"><span class="who">You</span>{turn.user}</p>
          {/if}
          {#if turn.assistant}
            <p class="line agent"><span class="who">Agent</span>{turn.assistant}</p>
          {/if}
        </div>
      {/each}
    {/if}
  </div>

  <footer class="history-foot">
    {#if enabled && turns.length > 0}
      Showing the last {RETAIN_LIMIT} exchanges; older entries are dropped as the log compacts.
    {:else}
      Stored in <code>uia-conversation.jsonl</code> next to your config. Nothing leaves this machine.
    {/if}
  </footer>
</div>

<style>
  .history {
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
    width: 100%;
    height: 100%;
    padding: 20px 24px;
    gap: 12px;
    color: var(--ink-1, #e8eaf0);
    font: 400 13px/1.5 system-ui, sans-serif;
  }

  .history-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }

  .history-head h2 {
    margin: 0;
    font-size: 15px;
    font-weight: 600;
  }

  .spacer {
    flex: 1;
  }

  /* Same chip as the HUD's, kept local rather than shared: the two cards have
     no common stylesheet, and a 22px square button is not worth one. */
  .chip {
    font: inherit;
    font-size: 12px;
    line-height: 1;
    color: var(--ink-4, #8890a0);
    background: rgba(var(--tint, 255, 255, 255), 0.06);
    border: 1px solid rgba(var(--tint, 255, 255, 255), 0.12);
    border-radius: 7px;
    width: 22px;
    height: 22px;
    /* Cancels the <button> default side padding; without it the icon overflows
       the ~8px content box and sits right of centre. */
    padding: 0;
    display: grid;
    place-items: center;
    cursor: pointer;
    flex-shrink: 0;
  }

  .chip-glyph {
    width: 14px;
    height: 14px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .chip:hover:not(:disabled) {
    color: var(--ink-1, #e8eaf0);
    background: rgba(var(--tint, 255, 255, 255), 0.12);
  }

  .chip:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .history-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    /* Text, unlike the rest of the deck, is worth being able to select and
       copy — the whole point of the card is reading something back. */
    -webkit-user-select: text;
    user-select: text;
  }

  .turn {
    padding: 8px 0;
    border-bottom: 1px solid rgba(var(--tint, 255, 255, 255), 0.08);
  }

  .turn:last-child {
    border-bottom: none;
  }

  .line {
    margin: 0 0 4px;
    /* Preserve the engine's own line breaks without collapsing runs of
       spaces into nothing, and wrap long unbroken strings rather than
       forcing the card to scroll sideways. */
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .line:last-child {
    margin-bottom: 0;
  }

  /* A fixed-width gutter rather than an inline prefix, so the text of every
     line starts on the same column and the exchange reads as a column of
     speech rather than as ragged prose. */
  .who {
    display: inline-block;
    width: 52px;
    flex-shrink: 0;
    font-size: 11px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--ink-4, #8890a0);
  }

  .line.agent .who {
    color: var(--accent, #6ea8fe);
  }

  .line.user {
    color: var(--ink-2, #cfd3dd);
  }

  .history-empty {
    margin: 24px 0;
    color: var(--ink-4, #8890a0);
    text-align: center;
  }

  .history-empty.error {
    color: #e8949a;
  }

  .history-foot {
    flex-shrink: 0;
    padding-top: 8px;
    border-top: 1px solid rgba(var(--tint, 255, 255, 255), 0.1);
    font-size: 11px;
    color: var(--ink-5, #6b7080);
  }

  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 10px;
  }
</style>
