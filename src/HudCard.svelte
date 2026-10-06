<script lang="ts">
  import { invoke, convertFileSrc } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';

  // S7. Replaces Hud.svelte inside the card deck: the HUD's own chrome
  // (status line, model badge, 9-bar equalizer, mic, text input, window
  // controls) rather than the pre-redesign status/meter/engine-buttons.
  //
  // Still presentation-only in the sense that matters: it renders what Rust
  // emits and calls commands, and makes no session decisions of its own.
  // Engine *selection* has moved off the HUD — S8's Providers tab owns it
  // on the same get_engine/set_engine commands.

  let {
    mini = false,
    onToggleMini,
    onOpenSettings,
    onOpenHistory,
  }: {
    mini?: boolean;
    onToggleMini: () => void;
    onOpenSettings: () => void;
    onOpenHistory: () => void;
  } = $props();

  const appWindow = getCurrentWindow();

  let status = $state('Idle');
  // Whether the session can hear us right now — Rust's answer, not ours to
  // infer. Rotation (a persona switch, the session-duration rotation, every
  // reconnect) disconnects before it reconnects, and for that window speech
  // is discarded rather than buffered. The status word alone never said so:
  // "Connecting" describes the socket, and the dot beside it stayed lit the
  // whole time, which is the HUD claiming to hear someone it cannot.
  //
  // Starts false to match the backend, which emits `Idle` at startup before
  // anything is connected. Assuming true would flash a live indicator over a
  // session that has not connected yet.
  let hearing = $state(false);
  let level = $state(0);
  // Queried on mount rather than left to the one-shot uia://engine emit,
  // which can race the webview's listen() registration (Hud.svelte's own
  // S21 finding — Tauri does not buffer events for a late listener).
  let engine = $state<'openai' | 'bedrock' | 'foundry'>('openai');
  let model = $state('');
  // S2's note: call get_capture_enabled on mount rather than assuming the
  // default — the webview reloads, the session does not.
  // Starts false to match the backend, which deliberately starts muted (see
  // `main.rs`'s `set_capture_enabled(false)`). `get_capture_enabled` below
  // is still the authority; this is only what renders until it resolves, and
  // as `true` it flashed "Listening" for a frame against a muted session.
  let captureEnabled = $state(false);
  let lastError = $state('');
  // The active persona's avatar - display-only in the full HUD, never
  // editable from here (Personas owns editing). Read once on mount; a
  // persona swap mid-session does not repaint this, same as the single
  // global avatar never used to update without a restart either.
  let avatarPath = $state<string | null>(null);

  // S0's capability signal (PLAN.md FD2/FD3): 'unknown' stays usable, only
  // 'unsupported' disables the input. Queried on mount (same race as
  // get_engine above) and kept live via the paired event.
  let textTurnSupport = $state<'supported' | 'unsupported' | 'unknown'>('unknown');
  let textTurnReason = $state<string | null>(null);
  const textTurnUnsupported = $derived(textTurnSupport === 'unsupported');

  invoke<string | null>('get_active_persona_avatar_path')
    .then((path) => {
      avatarPath = path;
    })
    .catch((e) => console.error('get_active_persona_avatar_path invoke() failed:', e));

  const ENGINE_LABEL = {
    openai: 'OpenAI',
    bedrock: 'Nova',
    foundry: 'Foundry',
  } as const;

  // All three engines answer `get_model` now, Bedrock included — it used to
  // be special-cased to a hardcoded id here because it had no config field.
  async function loadModel(current: 'openai' | 'bedrock' | 'foundry') {
    try {
      model = await invoke<string>('get_model', { provider: current });
    } catch (e) {
      console.error('get_model invoke() failed:', e);
      model = '';
    }
  }

  invoke<'openai' | 'bedrock' | 'foundry'>('get_engine')
    .then((current) => {
      engine = current;
      void loadModel(current);
    })
    .catch((e) => console.error('get_engine invoke() failed:', e));

  invoke<boolean>('get_capture_enabled')
    .then((enabled) => {
      captureEnabled = enabled;
    })
    .catch((e) => console.error('get_capture_enabled invoke() failed:', e));

  listen<{ state: string; hearing: boolean }>('uia://state', (event) => {
    const previous = status;
    status = event.payload.state;
    hearing = event.payload.hearing;
    // A transport error the session has already recovered from must stop
    // shouting. `lastError` used to be cleared in exactly one place — inside
    // `sendTyped` — so a blip that healed itself in seconds left a red line on
    // the HUD until the user happened to type something, telling them the
    // connection was broken long after it came back.
    //
    // Recovery is specifically Connecting/Idle -> Listening. Not "any arrival
    // at Listening": the assistant finishing a sentence is Speaking ->
    // Listening, and using that would wipe errors that are still true, like
    // Nova's "text turns are not supported" — which arrives on this same
    // channel while the session is perfectly healthy.
    //
    // `uia://connection` is not the signal to use here. It republishes the
    // connection-*wanted* gate (main.rs), so it says nothing about whether the
    // socket actually survived.
    if (status === 'Listening' && (previous === 'Connecting' || previous === 'Idle')) {
      lastError = '';
    }
    // S7: a handshake-derived capability answer (Foundry, always; OpenAI when
    // the field arrives late) resolves after connect() by definition, which
    // is after S0's own refresh points (Session::new/set_control/switch_engine
    // — deliberately not connect, PLAN.md FD3). 'Listening' is the session's
    // first ready state (Connecting -> EngineReady -> Listening,
    // uia-core/src/session/state.rs), so re-reading here is what actually
    // observes that late answer instead of relying only on the mount-time read.
    if (status === 'Listening') {
      refreshTextTurnSupport();
    }
  }).catch((e) => console.error('uia://state listen() failed:', e));

  listen<{ rms: number }>('uia://level', (event) => {
    level = Math.max(0, Math.min(1, event.payload.rms));
  }).catch((e) => console.error('uia://level listen() failed:', e));

  // Per S4's notes, an engine's "text turns are not supported" answer
  // arrives here asynchronously — it is not a rejected send_typed_turn
  // promise — so this listener is what surfaces the Nova case inline.
  // The session's connection gate. Republished by the backend rather than
  // inferred here, so the HUD can never claim a connection the session does
  // not have. It matters most after an idle disconnect, which the user did not
  // ask for: without this the mic would still read live against a session that
  // has gone, and speaking would do nothing.
  listen<{ connected: boolean }>('uia://connection', (event) => {
    if (!event.payload.connected) captureEnabled = false;
  }).catch((e) => console.error('uia://connection listen() failed:', e));
  listen<{ message: string }>('uia://error', (event) => {
    console.error('uia://error received:', event.payload.message);
    lastError = event.payload.message;
  }).catch((e) => console.error('uia://error listen() failed:', e));

  listen<{ engine: 'openai' | 'bedrock' | 'foundry' }>('uia://engine', (event) => {
    engine = event.payload.engine;
    void loadModel(event.payload.engine);
  }).catch((e) => console.error('uia://engine listen() failed:', e));

  function refreshTextTurnSupport() {
    invoke<{ support: 'supported' | 'unsupported' | 'unknown'; reason: string | null }>(
      'get_text_turn_support'
    )
      .then((payload) => {
        textTurnSupport = payload.support;
        textTurnReason = payload.reason;
      })
      .catch((e) => console.error('get_text_turn_support invoke() failed:', e));
  }

  refreshTextTurnSupport();

  listen<{ support: 'supported' | 'unsupported' | 'unknown'; reason: string | null }>(
    'uia://text-turn-support',
    (event) => {
      textTurnSupport = event.payload.support;
      textTurnReason = event.payload.reason;
    }
  ).catch((e) => console.error('uia://text-turn-support listen() failed:', e));

  // --- Waveform ----------------------------------------------------------
  // One flowing ribbon rather than nine bars. Layered curves share an
  // envelope that tapers to zero at both ends, so every layer converges on a
  // hairline rail at the edges and the shape reads as a voice rather than as
  // instrumentation. It fills the slot beside the avatar and ends at its own
  // left edge; the bars were a fixed 68px adrift in whatever space was left,
  // which is why the window resize stranded them off-centre.
  //
  // The engine sends one RMS number, never a spectrum, so this is a stylised
  // response to amplitude — as the bars were. What it must not do is invent
  // motion: a wave that undulates through silence is decoration pretending to
  // be your voice, so the driver below stops dead when nothing is being said.
  const LAYERS = 13;
  const VB_W = 300;
  const VB_H = 100;
  // Exponent on the end-taper. Higher pulls the wave further from the edges.
  const TAPER = 3;
  const FREQ = 2.2;
  // ~30fps. The HUD can sit visible for hours, so this is deliberately not
  // every frame the compositor offers.
  const FRAME_MS = 33;
  const PHASE_STEP = 0.16;
  // Below this the ribbon is flat anyway, so the loop stops rather than
  // redrawing an unchanging line forever.
  const SILENT_RMS = 0.03;

  const LAYER_OPACITY = Array.from({ length: LAYERS }, (_, i) =>
    (0.85 - (i / LAYERS) * 0.6).toFixed(2)
  );

  // RMS of speech sits low in the 0..1 range, so a linear map leaves the
  // ribbon barely moving. The cube root opens the quiet end out without ever
  // exceeding 1.
  const shaped = $derived(Math.cbrt(Math.min(1, level)));
  const live = $derived(captureEnabled && shaped > 0.04);

  function layerPath(amp: number, phase: number, layer: number): string {
    const steps = 48;
    const cy = VB_H / 2;
    // Inner layers reach highest, so the band has a bright spine and thins
    // outwards instead of reading as a stack of parallel lines.
    const reach = 1 - (layer / LAYERS) * 0.6;
    const offset = layer * 0.38;
    let d = '';
    for (let i = 0; i <= steps; i++) {
      const u = i / steps;
      const envelope = Math.pow(Math.sin(Math.PI * u), TAPER);
      const y =
        cy -
        envelope *
          amp *
          cy *
          reach *
          (Math.sin(u * FREQ * Math.PI * 2 + phase + offset) * 0.68 +
            Math.sin(u * FREQ * 1.93 * Math.PI * 2 + phase * 1.37 + offset) * 0.32);
      d += `${i ? 'L' : 'M'}${(u * VB_W).toFixed(1)} ${y.toFixed(1)}`;
    }
    return d;
  }

  function buildPaths(amp: number, phase: number): string[] {
    return Array.from({ length: LAYERS }, (_, i) => layerPath(amp, phase, i));
  }

  // Silence is a single flat line on the rail, not a row of stubs.
  const FLAT_PATHS = buildPaths(0, 0);
  let wavePaths = $state<string[]>(FLAT_PATHS);

  let phase = 0;
  let rafId = 0;
  let lastFrame = 0;

  // FD7: reduced motion is a baseline accessibility default, not a setting.
  // The ribbon still shows the level; it just stops travelling.
  const reducedMotion =
    typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

  function tick(now: number) {
    rafId = requestAnimationFrame(tick);
    if (now - lastFrame < FRAME_MS) return;
    lastFrame = now;
    phase += PHASE_STEP;
    wavePaths = buildPaths(shaped, phase);
  }

  function stopWave() {
    if (!rafId) return;
    cancelAnimationFrame(rafId);
    rafId = 0;
  }

  $effect(() => {
    // Reads `shaped` so it re-runs as the level moves, but starting and
    // stopping are idempotent: the loop is not restarted on every RMS event.
    const moving = showWave && shaped > SILENT_RMS;
    if (moving && !reducedMotion) {
      if (!rafId) {
        lastFrame = 0;
        rafId = requestAnimationFrame(tick);
      }
      return;
    }
    stopWave();
    // Under reduced motion the amplitude still shows, it just does not travel.
    wavePaths = moving ? buildPaths(shaped, 0) : FLAT_PATHS;
  });

  // Its own effect so the loop is torn down when the card goes away, rather
  // than on every re-run of the one above.
  $effect(() => stopWave);

  // --- Mic: FD11 push-to-talk -------------------------------------------
  // Short click toggles persistent listening; holding talks for as long as
  // the button is held, so a held mic can never be left pinned on.
  //
  // The press arms the mic *immediately* and the gesture is classified on
  // release, rather than the reverse. Waiting out the threshold before
  // arming — as this did originally — meant the first 350ms of every held
  // utterance was dropped on the floor, which is precisely the window in
  // which someone starts talking.
  const LONG_PRESS_MS = 350;
  let pressedAt = 0;
  // Whether this press is what turned the mic on, so a press that began on
  // an already-latched mic does not get treated as a hold-to-talk.
  let armedByPress = false;
  // Drives the glow: a held mic is lit but does not breathe, because the
  // bloom means "this will stay on after you let go".
  let holding = $state(false);

  function setCapture(enabled: boolean) {
    captureEnabled = enabled;
    // Infallible and returns nothing (S2's note), so the mouse-down/up path
    // can call it freely — there is no round-trip failure to handle.
    invoke('set_capture_enabled', { enabled }).catch((e) =>
      console.error('set_capture_enabled invoke() failed:', e)
    );
  }

  function handleMicDown() {
    pressedAt = performance.now();
    holding = true;
    armedByPress = !captureEnabled;
    if (armedByPress) setCapture(true);
  }

  function handleMicUp() {
    if (!holding) return;
    holding = false;
    // Under the threshold this was a click, so the mic latches on (or, if
    // the press began on an already-listening mic, that click turns it off).
    // At or over it, it was a hold, and a hold always ends when released.
    if (performance.now() - pressedAt < LONG_PRESS_MS) {
      if (!armedByPress) setCapture(false);
      return;
    }
    setCapture(false);
  }

  // Leaving the button mid-press ends the hold but never counts as the click
  // that would latch: releasing the mouse somewhere else is not a click here,
  // and a mic armed by a press that wandered off must not be left pinned on.
  function handleMicLeave() {
    if (!holding) return;
    holding = false;
    if (armedByPress) setCapture(false);
  }

  // Keyboard equivalent of the short click — the long-press gesture has no
  // meaningful key analogue, so space/enter toggle persistent listening.
  function handleMicKey(event: KeyboardEvent) {
    if (event.key !== ' ' && event.key !== 'Enter') return;
    event.preventDefault();
    setCapture(!captureEnabled);
  }

  // --- Typed turns (S4) --------------------------------------------------
  let draft = $state('');
  let sending = $state(false);

  // S11: a typed message otherwise vanishes the instant it's sent, with
  // nothing to confirm it actually went out - most noticeable exactly when
  // muted (the case that surfaces this input at all, per `showIO` below),
  // since there is no spoken echo of your own words to fall back on either.
  // Frontend-only, deliberately: the assistant's *replies* are voice, not
  // text, and captioning them would need real ASR/caption plumbing on the
  // engine side - out of scope here, this is only an outgoing-message log.
  // Both halves of the conversation, newest last, as one flat list of lines.
  //
  // This used to be `sentLog`: the last five things the user had *typed*,
  // frontend-only. That made the strip useless for the way the app is
  // actually used — speak to it and nothing appeared at all — so it now
  // merges two sources:
  //
  //   - what the user types, pushed locally the moment it is sent, because
  //     waiting for a round trip to see your own words is a lag you feel; and
  //   - `uia://turn`, one event per recorded exchange, which is where the
  //     assistant's side comes from and where a *spoken* user turn comes from.
  //
  // A typed exchange therefore arrives split: the user line locally, the
  // assistant line from the event, whose own user half is null (typed input
  // never becomes a `UserTranscript`). That is why the event's halves are
  // pushed independently rather than as a pair — pairing them would drop the
  // assistant's reply to anything typed.
  //
  // Still capped and still frontend-only: this is a trace of the session on
  // screen, not the record. `uia-conversation.jsonl` is the record, and the
  // history card reads it.
  const MAX_LOG = 8;
  type Line = { who: 'user' | 'agent'; text: string };
  let lines = $state<Line[]>([]);
  let logEl: HTMLButtonElement | undefined = $state();

  function pushLine(who: 'user' | 'agent', text: string | null | undefined) {
    const trimmed = text?.trim();
    if (!trimmed) return;
    lines = [...lines, { who, text: trimmed }].slice(-MAX_LOG);
  }

  listen<{ user: string | null; assistant: string | null }>('uia://turn', (event) => {
    pushLine('user', event.payload.user);
    pushLine('agent', event.payload.assistant);
  }).catch((e) => console.error('uia://turn listen() failed:', e));

  $effect(() => {
    lines;
    if (logEl) logEl.scrollTop = logEl.scrollHeight;
  });

  async function sendTyped() {
    const text = draft.trim();
    if (!text || sending) return;
    sending = true;
    lastError = '';
    try {
      await invoke('send_typed_turn', { text });
      pushLine('user', text);
      draft = '';
    } catch (e) {
      // Only a failure to *queue* the turn lands here (session gone, queue
      // full). The engine's own rejection — Nova's "not supported" — comes
      // back on uia://error instead.
      console.error('send_typed_turn invoke() failed:', e);
      lastError = String(e);
    } finally {
      sending = false;
    }
  }

  function handleDraftKey(event: KeyboardEvent) {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      void sendTyped();
    }
  }

  // --- Window controls (FD10) -------------------------------------------
  // ✕ calls window.close() and nothing else: hide-vs-quit lives entirely in
  // the Rust CloseRequested handler, so the button inherits whatever the
  // setting says.
  function closeWindow() {
    appWindow.close().catch((e) => console.error('window close() failed:', e));
  }

  function minimizeWindow() {
    appWindow.minimize().catch((e) => console.error('window minimize() failed:', e));
  }

  // The prototype's showWave/showIO: the wave and the text panel occupy the
  // same slot, and muting the mic is what swaps one for the other — but only
  // at full size. Compact mode is deliberately the mic, the window chips and
  // the equalizer and nothing else, so a muted compact HUD keeps the (flat)
  // wave rather than offering a text field there is no room to use.
  const showIO = $derived(!captureEnabled && !mini);
  const showWave = $derived(!showIO);

  // `Listening` is the core state machine's name for "connected, awaiting
  // speech" (uia-core/src/session/state.rs), and it is the right name there:
  // it describes the session, which knows nothing about the mic gate. It is
  // the wrong word to show a user whose microphone is off, though — the app
  // is not listening to them, and saying so while the dot beside it renders
  // muted is the UI contradicting itself.
  //
  // Only `Listening` is remapped. `Speaking` with the mic muted is still true
  // — that is the assistant talking, not the user.
  //
  // The deaf-window suffix is a second, independent rule: a session holding
  // no connection says so outright, whatever its state word is. `Idle` used
  // to be left bare here on the grounds that it "already means what it says
  // — no connection at all", which is true to someone who knows the state
  // machine and to nobody else. That is the same gap the window itself fell
  // into: the gap is short (a median 842 ms, measured in `rotation_gap.rs`)
  // and rarely hit, but a user who answered into it lost the sentence and
  // got no indication they had. Naming the consequence rather than the cause
  // is the point — "Connecting" is a fact about a socket that a person
  // cannot act on, while "not listening" tells them to wait before speaking.
  const statusLabel = $derived(
    !hearing
      ? `${status} — not listening`
      : status === 'Listening' && !captureEnabled
        ? 'Ready'
        : status
  );
</script>

<!-- Defined once and rendered in two places: the left column at full size,
     the single control row in compact mode. Duplicating the markup would mean
     duplicating four event handlers whose interaction (instant-arm press to
     talk, click to latch) is the fiddliest thing on the card. -->
{#snippet micButton()}
  <button
    type="button"
    class="mic"
    class:on={captureEnabled}
    class:latched={captureEnabled && !holding}
    aria-pressed={captureEnabled}
    aria-label={captureEnabled ? 'Mute microphone' : 'Unmute microphone'}
    title="Click to toggle listening; press and hold to talk"
    onmousedown={handleMicDown}
    onmouseup={handleMicUp}
    onmouseleave={handleMicLeave}
    onkeydown={handleMicKey}
  >
    <!-- One glyph for every state - muted, held and latched are told
         apart by fill and glow, not by swapping the icon out. -->
    <svg class="mic-glyph" viewBox="0 0 24 24" aria-hidden="true">
      <rect x="9" y="2" width="6" height="11" rx="3" />
      <path d="M5 10.5a7 7 0 0 0 14 0" />
      <line x1="12" y1="17.5" x2="12" y2="21" />
    </svg>
  </button>
{/snippet}

<div class="hud" class:mini>
  <div class="hud-top" data-tauri-drag-region>
    {#if !mini}
      <span class="hud-status" class:speaking={status === 'Speaking'} data-tauri-drag-region>
        <!-- `muted` is the mic being off, `deaf` is the session having no
             connection to send it to. Both mean "not being heard", so both
             stop the dot reading live; they are told apart because only one
             of them is the user's own doing and only one of them clears
             itself. -->
        <span class="dot" class:muted={!captureEnabled} class:deaf={!hearing && captureEnabled}></span>
        {statusLabel}
        <!-- The model id used to be a permanent badge on the control row.
             It is reference information — chosen in Settings, unchangeable
             while running — so it was the second most prominent text on the
             card for something almost never read. Here it is one hover from
             the indicator that already means "what is this session". -->
        <!-- Hidden by opacity rather than by `display`, so it stays in the
             accessibility tree and in find-in-page: a screen reader reads the
             status as "Ready, OpenAI, gpt-realtime-2.1-mini" without needing
             to discover a hover. The reveal is a convenience for people using
             their eyes, which is why this is not focusable — it is text about
             the session, not a control, and giving it a tab stop would
             promise an interaction that does not exist. -->
        <span class="hud-model-pop">
          <span class="pop-engine">{ENGINE_LABEL[engine]}</span>
          <span class="pop-model">{model || 'unknown model'}</span>
        </span>
      </span>
    {/if}

    <!-- Errors take the top row now that the control row is gone. They sit
         after the status because that is what they are: the most urgent thing
         the session has to say about itself. Truncated to whatever the chips
         leave, with the full text in the tooltip. -->
    {#if lastError}
      <!-- Dismissible, because not every error has a recovery to clear it.
           The listener above clears this when the session reconnects, which
           covers transport failures; an error that arrives while the session
           stays healthy never changes the state and would otherwise sit here
           forever. Nova's "text turns are not supported" is the real case. -->
      <button
        type="button"
        class="hud-error"
        title="{lastError} — click to dismiss"
        onclick={() => (lastError = '')}>{lastError}</button
      >
    {:else if textTurnUnsupported && textTurnReason}
      <p class="hud-error" title={textTurnReason}>{textTurnReason}</p>
    {/if}

    <span class="spacer" data-tauri-drag-region></span>

    <!-- Chip icons are inline SVG, not text glyphs. A glyph such as ▢ or ⚙ is
         drawn by whichever font the OS substitutes, so its side bearings and
         baseline differ per platform and it sat visibly off-centre on macOS.
         SVG on a 24x24 grid is centred by the chip's own `place-items: center`
         and looks the same everywhere. The collapse chip shows the ACTION
         (arrows in to collapse, out to expand), not the state, and so cannot
         be mistaken for the window-minimize dash beside it. -->
    <button
      type="button"
      class="chip"
      aria-label={mini ? 'Expand HUD' : 'Collapse HUD'}
      title={mini ? 'Expand' : 'Collapse'}
      onclick={onToggleMini}
    >
      <svg class="chip-glyph" viewBox="0 0 24 24" aria-hidden="true">
        {#if mini}
          <polyline points="15 3 21 3 21 9" />
          <polyline points="9 21 3 21 3 15" />
          <line x1="21" y1="3" x2="14" y2="10" />
          <line x1="3" y1="21" x2="10" y2="14" />
        {:else}
          <polyline points="4 14 10 14 10 20" />
          <polyline points="20 10 14 10 14 4" />
          <line x1="14" y1="10" x2="21" y2="3" />
          <line x1="3" y1="21" x2="10" y2="14" />
        {/if}
      </svg>
    </button>
    <button
      type="button"
      class="chip"
      aria-label="Settings"
      title="Settings"
      onclick={onOpenSettings}
    >
      <svg class="chip-glyph" viewBox="0 0 24 24" aria-hidden="true">
        <circle cx="12" cy="12" r="3.5" />
        <circle cx="12" cy="12" r="7" />
        <line x1="12" y1="5" x2="12" y2="2" />
        <line x1="12" y1="19" x2="12" y2="22" />
        <line x1="5" y1="12" x2="2" y2="12" />
        <line x1="19" y1="12" x2="22" y2="12" />
        <line x1="16.95" y1="7.05" x2="19.07" y2="4.93" />
        <line x1="7.05" y1="7.05" x2="4.93" y2="4.93" />
        <line x1="16.95" y1="16.95" x2="19.07" y2="19.07" />
        <line x1="7.05" y1="16.95" x2="4.93" y2="19.07" />
      </svg>
    </button>
    <button
      type="button"
      class="chip"
      aria-label="Minimize"
      title="Minimize"
      onclick={minimizeWindow}
    >
      <svg class="chip-glyph" viewBox="0 0 24 24" aria-hidden="true">
        <line x1="5" y1="12" x2="19" y2="12" />
      </svg>
    </button>
    <button type="button" class="chip close" aria-label="Close" title="Close" onclick={closeWindow}>
      <svg class="chip-glyph" viewBox="0 0 24 24" aria-hidden="true">
        <line x1="6" y1="6" x2="18" y2="18" />
        <line x1="18" y1="6" x2="6" y2="18" />
      </svg>
    </button>
  </div>

  <div class="hud-slot" data-tauri-drag-region>
    {#if !mini}
      <!-- The avatar floats centred in the slot; the mic is pinned to the
           bottom of this column, on the text input's line and at the input's
           height. Both are positioned against this column rather than flowing
           inside it, because they answer to different things: the avatar to
           the slot's centre, the mic to the input's top edge. -->
      <div class="hud-leftcol" class:no-avatar={!avatarPath}>
        {#if avatarPath}
          <!-- Frame and image are separate elements: the frame draws the rim
               and shadows, the image sits under them. One element cannot do
               both, since a filter on the image would dim the frame it draws
               too. -->
          <div class="hud-avatar-frame" aria-hidden="true">
            <img class="hud-avatar" src={convertFileSrc(avatarPath)} alt="" />
          </div>
        {/if}
        {@render micButton()}
      </div>
    {/if}

    <div class="hud-slot-content">
      {#if showWave}
        <svg
          class="hud-wave"
          class:live
          viewBox="0 0 {VB_W} {VB_H}"
          preserveAspectRatio="none"
          aria-hidden="true"
          data-tauri-drag-region
        >
          <line class="wave-rail" x1="0" y1={VB_H / 2} x2={VB_W} y2={VB_H / 2} />
          {#each wavePaths as d, i (i)}
            <path {d} stroke-opacity={LAYER_OPACITY[i]} />
          {/each}
        </svg>
      {/if}

      {#if showIO}
        <div class="hud-io">
          <!-- The recent-messages strip is also the way into the full
               conversation, rather than a fifth chip on the top row: it is
               already the thing on screen that means "what has been said",
               and the card it opens is the same idea at length.

               Rendered even when empty, which the strip did not used to be.
               `sentLog` holds only what the user has *typed* this session, so
               a spoken-only session left nothing here — and gating the
               affordance on it would make stored history unreachable for
               exactly the users most likely to have some. -->
          <button
            type="button"
            class="hud-io-log"
            class:empty={lines.length === 0}
            bind:this={logEl}
            title="Open the full conversation"
            aria-label="Open the full conversation"
            onclick={onOpenHistory}
          >
            {#if lines.length > 0}
              {#each lines as line, i (i)}
                <span class={line.who}>{line.text}</span>
              {/each}
            {:else}
              <span class="hud-io-log-hint">Conversation history</span>
            {/if}
          </button>
          <div class="hud-io-row">
            <input
              type="text"
              bind:value={draft}
              onkeydown={handleDraftKey}
              disabled={textTurnUnsupported}
              placeholder={textTurnUnsupported ? 'Text input unavailable' : 'Type a message…'}
              aria-label="Send a typed message"
            />
            <button
              type="button"
              onclick={sendTyped}
              disabled={sending || draft.trim() === '' || textTurnUnsupported}
            >
              {sending ? '…' : 'Send'}
            </button>
          </div>
        </div>
      {/if}
    </div>
  </div>

  <!-- Compact mode only. At full size the mic lives in the left column and
       the model id lives in the status popover, so this row had nothing left
       in it — deleting it gave the conversation strip 36px, which is about
       two and a half more lines. Compact has no avatar to hang the mic on, so
       it keeps the row. -->
  {#if mini}
    <div class="hud-controls">
      {@render micButton()}
    </div>
  {/if}
</div>

<style>
  .hud {
    box-sizing: border-box;
    width: 100%;
    height: 100%;
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px 14px;
    /* The height of the text-input row, declared rather than left to the
       input's intrinsic size, because the microphone is sized and positioned
       from it: the mic is exactly this tall and sits on this row's line, so
       the two read as one horizontal band across the card. Left intrinsic, a
       font or padding change would silently pull them apart. */
    --io-row-h: 30px;
    color: var(--ink-1, #e8eaf0);
    font: 400 13px/1.4 system-ui, sans-serif;
  }

  /* Compact mode is a pill, not a shrunken card: one row of mic, wave and
     window chips, with the wave taking whatever is left between them. The
     stacked layout only makes sense once there is a status line and a badge
     to stack. */
  .hud.mini {
    flex-direction: row;
    align-items: center;
    gap: 10px;
    padding: 0 10px;
  }

  .hud.mini .hud-controls {
    order: 1;
  }

  .hud.mini .hud-slot {
    order: 2;
    flex: 1;
    height: 26px;
  }

  /* Compact mode is the wave and the window chips, with no avatar and no
     text input, so there is nothing for the offset above to align to and a
     30px lift would push the wave out of a 26px row entirely. */
  .hud.mini .hud-wave {
    height: 100%;
    margin-bottom: 0;
  }

  .hud.mini .hud-top {
    order: 3;
    flex: 0 0 auto;
  }


  .hud-top {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }

  .spacer {
    flex: 1;
  }

  .hud-status {
    position: relative;
    display: inline-flex;
    align-items: center;
    gap: 6px;

    letter-spacing: 0.02em;
    color: var(--ink-2, #cfd3dd);
    white-space: nowrap;
  }

  /* Shown on hover and on keyboard focus, never on click: this is reference
     information, so reaching for it should cost nothing and dismissing it
     should not be a thing you have to do. A native `title` would have been
     free, but it arrives after a second, in an OS-coloured box, which on this
     card reads as a bug rather than as a tooltip. */
  .hud-model-pop {
    position: absolute;
    top: calc(100% + 6px);
    left: -4px;
    z-index: 5;
    min-width: 140px;
    padding: 7px 10px;
    border-radius: 9px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    opacity: 0;
    transform: translateY(-3px);
    pointer-events: none;
    transition:
      opacity 120ms ease,
      transform 120ms ease;
    background: linear-gradient(
      160deg,
      color-mix(in srgb, var(--panel-glass-1, rgba(40, 44, 57, 0.96)) 100%, transparent),
      var(--panel-glass-3, rgba(13, 15, 20, 0.97))
    );
    border: 1px solid rgba(var(--tint, 255, 255, 255), 0.18);
    box-shadow:
      0 10px 26px rgba(0, 0, 0, 0.5),
      0 1px 0 rgba(var(--tint, 255, 255, 255), 0.1) inset;
  }

  .hud-status:hover .hud-model-pop {
    opacity: 1;
    transform: translateY(0);
  }

  .pop-engine {
    font-size: 9.5px;
    letter-spacing: 0.09em;
    text-transform: uppercase;
    color: var(--ink-4, #8890a0);
  }

  .pop-model {
    font-size: 11.5px;
    color: var(--ink-1, #e8eaf0);
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }

  .hud-status.speaking {
    color: var(--accent, #6ea8fe);
  }

  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--accent, #6ea8fe);
    box-shadow: 0 0 8px color-mix(in srgb, var(--accent, #6ea8fe) 80%, transparent);
  }

  .dot.muted {
    background: var(--ink-5, #6b7080);
    box-shadow: none;
  }

  /* Hollow rather than grey: a muted mic is off and settled, a deaf session
     is mid-something and will come back on its own. The ring says "not now"
     without saying "broken". */
  .dot.deaf {
    box-sizing: border-box;
    background: transparent;
    box-shadow: none;
    border: 1.5px solid var(--ink-5, #6b7080);
  }

  /* Display only — the engine and model are chosen in Settings (S8), never
     from the HUD. */
  .hud-model {
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 11px;
    color: var(--ink-4, #8890a0);
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 999px;
    padding: 2px 8px;
  }

  .chip {
    font: inherit;
    font-size: 12px;
    line-height: 1;
    color: var(--ink-4, #8890a0);
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 7px;
    width: 22px;
    height: 22px;
    /* A <button> carries a default ~6px side padding. At 22px wide that leaves
       ~8px of content box, so a 14px icon overflows it to the right and looks
       right-aligned. */
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

  .chip:hover {
    color: var(--ink-1, #e8eaf0);
    background: rgba(255, 255, 255, 0.12);
  }

  .chip.close:hover {
    background: #d0424a;
    border-color: #d0424a;
    color: #fff;
  }

  /* The wave and the text panel share one slot: muting swaps one for the
     other, so the card's height never changes with mic state. */
  .hud-slot {
    flex: 1;
    min-height: 0;
    display: flex;
    /* `stretch`, so the left column spans the whole slot and the avatar and
       mic can be positioned against its top and bottom. Children that want
       to be centred say so themselves. */
    align-items: stretch;
    gap: 10px;
  }

  /* The mic holds the floor, on the text input's line; the avatar centres in
     everything left above it.

     That "above it" is the whole point. Centring the avatar on the *slot*
     — which is what this did first — counts the mic's band as space the
     avatar may sit in, so the face lands ~15px low and its bottom edge comes
     within 10px of the mic. Centring on the free space instead puts the
     avatar where the eye expects it and opens the gap to ~25px, without
     either number being written down: `margin: auto` on the avatar absorbs
     what the mic leaves, so the spacing follows the card if it ever resizes. */
  .hud-leftcol {
    display: flex;
    flex-direction: column;
    flex-shrink: 0;
    width: 80px;
  }

  /* No avatar configured: the column shrinks to the mic, rather than leaving
     80px of nothing beside the conversation. */
  .hud-leftcol.no-avatar {
    width: var(--io-row-h);
  }

  /* Fixed square, cropped rather than stretched (`object-fit: cover`) so an
     arbitrary source image never distorts - sized to the full HUD's own
     slot height so it reads as the HUD's face, not a decoration. Compact
     mode has no room for it (`!mini` in the markup). */
  /* A single faint hairline could not contain an uploaded photo: a bright
     image ran to its own edge and sat on the glass like a sticker. This is
     the card's own edge-light scaled down — rim, top highlight, and a dark
     shadow rising from the bottom, which is the part that actually seats the
     image *in* the material. Drawn on a pseudo-element so the frame is never
     affected by anything applied to the image itself. */
  /* 62px was sized for the 306x190 window and read as a stamp once the card
     grew to 342x213 — the slot is about 125px tall, so it was filling half of
     it. 80px sits in the same proportion to the taller slot, and still leaves
     the ribbon ~224px of the row. */
  .hud-avatar-frame {
    position: relative;
    flex-shrink: 0;
    /* `auto` on all four sides: in a column flex container this absorbs the
       free space evenly, which centres the avatar in whatever the mic below
       has not taken. Still `relative`, because the frame's rim and shadows
       are drawn on a pseudo-element positioned against it. */
    position: relative;
    margin: auto;
    width: 80px;
    height: 80px;
    border-radius: 12px;
    overflow: hidden;
  }

  .hud-avatar-frame::after {
    content: '';
    position: absolute;
    inset: 0;
    border-radius: inherit;
    pointer-events: none;
    /* Rim and highlight ride on --tint so they invert in light mode, which the
       hardcoded white border they replace never did. The shadow stays black:
       shadows do not invert. */
    box-shadow:
      0 0 0 1px rgba(var(--tint, 255, 255, 255), 0.22) inset,
      0 1px 0 rgba(var(--tint, 255, 255, 255), 0.2) inset,
      0 -9px 16px rgba(0, 0, 0, 0.38) inset;
  }

  .hud-avatar {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .hud-slot-content {
    flex: 1;
    min-width: 0;
    height: 100%;
    display: flex;
    align-items: center;
  }

  .hud-wave {
    display: block;
    width: 100%;
    /* Exactly the avatar's box: same height, and centred in the same reduced
       space. The bottom margin is what does that — flex centring works on the
       margin box, so reserving the mic's height below the wave lands it on
       the avatar's centre line rather than the slot's. Without it the ribbon
       rail would cross the face ~15px below its middle.

       `max-height` rather than a bare 80px because compact mode reuses this
       in a 26px row, where the cap wins; the margin goes there too, since
       compact has no mic column to balance against. */
    height: 80px;
    max-height: 100%;
    align-self: center;
    margin-bottom: var(--io-row-h);
    /* Bloom, kept low: this sits on smoked glass, not on black. */
    filter: drop-shadow(0 0 3px color-mix(in srgb, var(--accent, #6ea8fe) 60%, transparent));
  }

  .hud-wave path {
    fill: none;
    stroke: var(--accent, #6ea8fe);
    stroke-width: 0.7;
    /* The viewBox is stretched to the slot (`preserveAspectRatio: none`), so
       without this the strokes would be scaled with it and come out thicker
       horizontally than vertically. */
    vector-effect: non-scaling-stroke;
  }

  /* What the ribbon decays into, and all there is to see in silence. */
  .wave-rail {
    stroke: color-mix(in srgb, var(--accent, #6ea8fe) 55%, transparent);
    stroke-width: 1;
    vector-effect: non-scaling-stroke;
  }

  /* The input row sits at the bottom of the slot rather than floating in the
     middle of it: the HUD grew (306x190 -> 342x213) precisely to give the
     log room, and centring the pair would have split that new height evenly
     above and below the input instead of handing it to the log. */
  .hud-io {
    display: flex;
    flex-direction: column;
    justify-content: flex-end;
    gap: 4px;
    width: 100%;
    height: 100%;
  }

  /* Newest at the bottom, oldest scrolled out of view. It takes whatever the
     input row leaves rather than the flat 34px cap it had while the slot was
     centred - roughly two lines before, five or six now. */
  .hud-io-log {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    justify-content: flex-end;
    gap: 3px;
    /* Older lines dissolve into the glass rather than being sliced at the top
       edge. That slice is what made the strip read as a box even with no
       border drawn: a line cut through its own letterforms is a hard edge
       wherever it lands. The mask has no such edge, so the strip has no shape
       of its own. */
    -webkit-mask-image: linear-gradient(to bottom, transparent 0%, #000 52%, #000 100%);
    mask-image: linear-gradient(to bottom, transparent 0%, #000 52%, #000 100%);
    /* Undoing the button chrome. It is a button so that opening the history
       is keyboard reachable without inventing a key handler, but it must go
       on looking like the quiet strip of text it was — no box, no fill, no
       border, nothing that reads as a control sitting on the glass.
       Deliberately no hover state either: a strip this size lighting up under
       the pointer is movement in the corner of the eye every time the cursor
       crosses the card, and the cursor and tooltip already say it is
       clickable. */
    appearance: none;
    border: none;
    background: none;
    padding: 0;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }

  /* Keyboard focus is the one exception, and it is not a hover: this appears
     only when the strip is reached by Tab, never on a click. Removing it
     would leave a keyboard user with no way to see where they are. */
  .hud-io-log:focus-visible {
    outline: 1px solid color-mix(in srgb, var(--accent, #6ea8fe) 55%, transparent);
    outline-offset: 2px;
    border-radius: 4px;
  }

  /* Nothing typed yet: the strip collapses to a single quiet label rather
     than reserving five empty lines above the input. */
  .hud-io-log.empty {
    flex: 0 0 auto;
  }

  .hud-io-log span {
    display: block;
    margin: 0;
    font-size: 11px;
    line-height: 1.3;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--ink-3, #aeb3c0);
  }

  /* Who spoke, carried by colour rather than by a label or a gutter: there
     are ~224px to work with, and "You"/"Agent" prefixes would spend a fifth
     of every line saying something the tint says for free. */
  .hud-io-log span.agent {
    color: color-mix(in srgb, var(--accent, #6ea8fe) 62%, var(--ink-3, #aeb3c0));
  }

  /* The newest line is the one actually read, so it gets two lines of room at
     full strength while everything above it stays a single-line trace. At
     11px in 224px an assistant reply is otherwise four words and an ellipsis
     — enough to know it replied, not enough to know what it said. */
  .hud-io-log span:last-child {
    white-space: normal;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    color: var(--ink-1, #e8eaf0);
  }

  .hud-io-log span.agent:last-child {
    color: color-mix(in srgb, var(--accent, #6ea8fe) 78%, var(--ink-1, #e8eaf0));
  }

  .hud-io-log-hint {
    color: var(--ink-5, #6b7080);
  }

  .hud-io-log::-webkit-scrollbar {
    width: 4px;
  }

  .hud-io-log::-webkit-scrollbar-thumb {
    background: rgba(255, 255, 255, 0.16);
    border-radius: 4px;
  }

  .hud-io-row {
    display: flex;
    /* `stretch`, so the input and the send button are exactly --io-row-h tall
       rather than their intrinsic height centred inside it. The row's top
       edge is a line other things align to, so it should be the input's real
       top edge and not half a pixel above it. */
    align-items: stretch;
    gap: 6px;
    width: 100%;
    flex-shrink: 0;
    height: var(--io-row-h);
  }

  .hud-io input {
    flex: 1;
    min-width: 0;
    font: inherit;
    color: var(--ink-1, #e8eaf0);
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 5px 9px;
  }

  .hud-io input::placeholder {
    color: var(--ink-5, #6b7080);
  }

  .hud-io input:focus {
    outline: none;
    border-color: var(--accent, #6ea8fe);
  }

  .hud-io input:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* Scoped to the input row, not to `.hud-io`, because the log strip above it
     is a button too. As `.hud-io button` these rules dressed the strip as a
     send button: the base look was overridden by `.hud-io-log` and looked
     right, but `:hover` is more specific than that class and won, so passing
     the cursor over the strip painted the whole thing accent-filled with dark
     text. A rule written for one button in a container quietly acquired a
     second one. */
  .hud-io-row button {
    font: inherit;
    font-size: 12px;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 5px 10px;
    cursor: pointer;
  }

  .hud-io-row button:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .hud-io-row button:not(:disabled):hover {
    background: var(--accent, #6ea8fe);
    border-color: var(--accent, #6ea8fe);
    color: #0c0e12;
  }

  .hud-controls {
    flex-shrink: 0;
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }

  /* A round icon button rather than a labelled pill. The label was the
     widest thing on a control row with none to spare, and since the glyph no
     longer changes between states, fill and glow carry the meaning instead —
     `aria-label` still says which state it is for anyone not looking. */
  /* On the input's line and at the input's height, so the bottom of the card
     is one continuous band: mic, then text field, then send. Positioned
     against the left column rather than flowed, since the avatar above it is
     centred and the two cannot share one flow anchor.

     Only at full size — compact mode renders the same button inside its own
     control row, where it stays in normal flow at its natural size. */
  .hud-leftcol .mic {
    flex-shrink: 0;
    /* Horizontally centred under the avatar; vertically it simply sits last
       in the column, which is the slot's floor. */
    margin: 0 auto;
    width: var(--io-row-h);
    height: var(--io-row-h);
  }

  .mic {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    padding: 0;
    color: var(--ink-3, #aeb3c0);
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 999px;
    cursor: pointer;
    flex-shrink: 0;
    transition:
      color 160ms ease,
      background-color 160ms ease,
      border-color 160ms ease;
  }

  .mic:hover {
    color: var(--ink-1, #e8eaf0);
  }

  /* Lit but not blooming. This is what a held (push-to-talk) mic looks like:
     the bloom below is a promise that the mic stays open after you let go,
     so a press that ends in silence must never show it. */
  .mic.on {
    color: #0c0e12;
    background: var(--accent, #6ea8fe);
    border-color: var(--accent, #6ea8fe);
  }

  /* Latched: the same fill, plus a bloom that breathes so an open mic stays
     noticeable in peripheral vision on an always-on overlay. */
  .mic.latched {
    animation: micBreath 2600ms ease-in-out infinite alternate;
  }

  @keyframes micBreath {
    from {
      box-shadow: 0 0 6px -1px color-mix(in srgb, var(--accent, #6ea8fe) 45%, transparent);
    }
    to {
      box-shadow: 0 0 13px 1px color-mix(in srgb, var(--accent, #6ea8fe) 85%, transparent);
    }
  }

  .mic-glyph {
    width: 15px;
    height: 15px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
  }

  .hud-error {
    /* Sits between the status and the window chips, so it takes whatever is
       left of that row rather than pushing the chips off the card. */
    flex: 1;
    min-width: 0;
    /* A button rather than a paragraph, so dismissing it is keyboard
       reachable for free. Everything below the cursor rule is undoing the
       button chrome the browser supplies. */
    appearance: none;
    border: none;
    background: none;
    padding: 0;
    text-align: left;
    cursor: pointer;
    font-family: inherit;
    margin: 0;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 11px;
    color: #e8949a;
  }

  /* FD7: reduced motion is a baseline accessibility default, not a setting.
     The bars still track the real level — only the shimmer stops. */
  @media (prefers-reduced-motion: reduce) {
    /* The ribbon's driver already refuses to run under reduced motion (see
       `reducedMotion` in the script); it still shows the level, it just does
       not travel. Nothing to disable here. */

    /* The bloom is the live-mic signal, so this stops it breathing rather
       than removing it: it settles at the bright end of the cycle. Dropping
       it outright would leave an open mic looking the same as a held one. */
    .mic.latched {
      animation: none;
      box-shadow: 0 0 13px 1px color-mix(in srgb, var(--accent, #6ea8fe) 85%, transparent);
    }
  }
</style>
