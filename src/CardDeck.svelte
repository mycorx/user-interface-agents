<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow, LogicalSize } from '@tauri-apps/api/window';
  import HudCard from './HudCard.svelte';
  import HistoryCard from './HistoryCard.svelte';
  import SettingsCard from './SettingsCard.svelte';

  // Mirrors `uia_app::settings::UiSettings` - duplicated across the IPC
  // boundary like the other settings tabs' types. `theme` of `null` means
  // "follow prefers-color-scheme" (S9).
  // `glass_tint` (S11): `null`/`"rim"` for an accent-colored border/edge-light
  // only, or `"wash"` to also blend the accent into the glass fill itself.
  type UiSettings = { theme: string | null; accent: string | null; glass_tint: string | null };

  // The app root. Owns the card-deck stack (HUD is the base card, Settings
  // stacks on top of it in false 3D), the uia://open-settings listener,
  // and the window-resize effect. Per PLAN.md FD13(a) it originally wrapped
  // the *existing* Hud.svelte and Settings.svelte; both have since been
  // replaced by their own cards — S6's SettingsCard.svelte and S7's
  // HudCard.svelte. S7 also added the mini/full HUD state the size toggle
  // in HudCard drives.

  // The prototype exposed this as a tunable design-canvas prop; the real app
  // has no such control, so its default (1) is the value we ship.
  const DEPTH_MAG = 1;
  // Must stay in lockstep with the panelSuckIntoCog duration below: the
  // layer is only dropped from the stack once the exit animation has
  // actually finished, since an instant unmount would skip it entirely.
  const CLOSE_ANIM_MS = 360;

  // FD1 (as amended by S12, and again by S11 below): the hud window is a
  // real fixed-footprint overlay, so sizing is ours to solve — and each box
  // now *hugs* its card exactly rather than leaving margin around it. Under
  // a native window effect (FD16) the margin is not empty, it is frosted,
  // so any window larger than its card shows the effect around it as a
  // rectangle. S7 splits 'hud' into the mini/full pair and sizes both the
  // same way. The mini box is a single pill row — mic, wave, window chips —
  // with the status line, the model badge and the text panel all dropped.
  // S11 grew hudFull's height (156 -> 190): the avatar (62px square) and
  // the typed-message log it added both live in the same slot as the
  // equalizer/text-input, and 156px read as cramped once they did.
  //
  // Settings widened 424 -> 600 for the MCP tab's server table. At 424 the
  // panel had 376px of content (padding is 20px 24px) against a table
  // needing ~495px, so it scrolled sideways; 520 was still ~23px short,
  // which showed as a scrollbar rather than as a near miss. Only the
  // settings box grows — the HUD keeps its own 342px footprint, since this
  // size applies solely while Settings is open.
  //
  // The HUD itself went 306 -> 342 wide (190 -> 213 / 52 -> 58 tall): the
  // control row was the cramped part, with the mic, the model badge and the
  // status all competing inside 306px. Note this moves the cog, which the
  // genie animation's `transform-origin` is derived from — see below.
  //
  // Note the floor comes from the COLUMN HEADERS, not the cells: "ENABLED"
  // reserves ~60px above a 13px checkbox. Width alone still does not settle
  // it, because server names and resolved launch paths are unbounded — the
  // table also wraps and scrolls inside itself (see SettingsMcp.svelte).
  // This is the headroom that keeps that backstop from being reached in
  // normal use.
  const SIZES = {
    hudMini: { w: 342, h: 58 },
    hudFull: { w: 342, h: 213 },
    settings: { w: 600, h: 584 },
    // Matches settings deliberately. The history card is a scrollback and
    // would take any height offered; sharing the settings footprint means
    // opening either from the HUD is the same window movement, and the deck
    // never resizes when moving between them.
    history: { w: 600, h: 584 },
  } as const;

  type SizeKind = keyof typeof SIZES;
  // The deck stacks the HUD plus at most one overlay; which *size* the HUD
  // card takes is a separate axis (mini vs full), so the layer kind is not
  // the size key.
  type OverlayKind = 'settings' | 'history';
  type LayerKind = 'hud' | OverlayKind;

  // FD17: the card's radius must never exceed the radius the OS rounds the
  // *window* to. Smaller is always safe — the window simply clips the card,
  // and the window's own shape is what the eye reads. Larger is the bug S12
  // fixed: the corners outside the card's curve are still window, so the
  // native effect shows there as square shoulders. These are therefore
  // ceilings, not preferences.
  const CARD_RADIUS = {
    // Windows 11 rounds undecorated windows at 8px under `shadow: true`.
    windows11: 8,
    // Windows 10 does not round at all, but still applies acrylic — so any
    // radius above 0 would show the effect in the corners.
    windows10: 0,
    // macOS does not round undecorated windows, so the effect would show as
    // square corners. tauri.conf.json sets `windowEffects.radius` (macOS-only;
    // Windows ignores it) to round the glass itself. Keep the two equal.
    macos: 12,
    // No native effect is applied (Linux, or any platform where the effect
    // is unsupported), so the window stays genuinely transparent, nothing
    // is composited behind the corners, and the prototype's own 22px is
    // both safe and the intended look.
    none: 22,
  } as const;

  let cardRadius = $state<number>(CARD_RADIUS.windows10);

  // S9: the deck's default (dark, --accent's existing #6ea8fe) until
  // `get_ui_settings` resolves, so nothing flashes an unstyled state.
  const DEFAULT_ACCENT = '#6ea8fe';
  let uiSettings = $state<UiSettings>({ theme: null, accent: null, glass_tint: null });

  // `theme: null` means "follow the OS" (S3's `UiSettings` doc comment) -
  // this is the one place that fallback is resolved, since it is a
  // browser-side query with no Rust equivalent.
  function resolveLightMode(theme: string | null): boolean {
    if (theme === 'light') return true;
    if (theme === 'dark') return false;
    return window.matchMedia('(prefers-color-scheme: light)').matches;
  }

  const lightMode = $derived(resolveLightMode(uiSettings.theme));
  const accent = $derived(uiSettings.accent ?? DEFAULT_ACCENT);
  const glassWash = $derived(uiSettings.glass_tint === 'wash');

  $effect(() => {
    void invoke<UiSettings>('get_ui_settings')
      .then((settings) => {
        uiSettings = settings;
      })
      .catch((e) => console.error('get_ui_settings invoke() failed:', e));
  });

  // Windows 10 and 11 both report "Windows NT 10.0" in the UA string, so the
  // UA alone cannot tell them apart. UA-CH's platformVersion can: Microsoft
  // maps Windows 11 to major >= 13. WebView2 is Chromium, so this is
  // available without adding the os plugin — which would mean a new Rust
  // dependency and a main.rs registration, and main.rs is shared territory.
  async function detectCardRadius(): Promise<number> {
    const uaData = (
      navigator as Navigator & {
        userAgentData?: {
          platform?: string;
          getHighEntropyValues?: (h: string[]) => Promise<{ platformVersion?: string }>;
        };
      }
    ).userAgentData;

    const platform = uaData?.platform ?? '';
    const ua = navigator.userAgent;
    const isWindows = platform ? platform === 'Windows' : /Windows/.test(ua);
    const isMac = platform ? platform === 'macOS' : /Mac OS X|Macintosh/.test(ua);

    if (isWindows) {
      try {
        const high = await uaData?.getHighEntropyValues?.(['platformVersion']);
        const major = Number.parseInt(String(high?.platformVersion ?? '').split('.')[0], 10);
        // Unknown version falls through to the Windows 10 value: the floor
        // is never wrong-looking, only ever less rounded than it could be.
        return Number.isFinite(major) && major >= 13
          ? CARD_RADIUS.windows11
          : CARD_RADIUS.windows10;
      } catch {
        return CARD_RADIUS.windows10;
      }
    }
    if (isMac) return CARD_RADIUS.macos;
    return CARD_RADIUS.none;
  }

  $effect(() => {
    void detectCardRadius()
      .then((r) => {
        cardRadius = r;
      })
      .catch((e) => console.error('card radius detection failed:', e));
  });

  interface Layer {
    kind: LayerKind;
    jumpable: boolean;
    innerClass: string;
    shellStyle: string;
    innerStyle: string;
  }

  // One overlay at a time, by construction: opening either replaces the
  // other rather than stacking a third card, which keeps the deck's depth
  // arithmetic (and the receded-HUD jump target) a two-layer problem.
  let overlay = $state<OverlayKind | null>(null);
  let closingOverlay = $state(false);
  // The HUD's size toggle (S7). An overlay always opens over the full-size
  // window, so this only decides the HUD-only footprint.
  let miniHud = $state(false);
  let closeTimer: ReturnType<typeof setTimeout> | null = null;

  const stack = $derived<LayerKind[]>(overlay ? ['hud', overlay] : ['hud']);

  // Ported verbatim from the prototype's renderVals(). Depth 0 is the
  // focused card at full clarity; depth 1 (the HUD behind an open Settings)
  // is pushed heavily back and blurred so it reads as "barely visible"
  // rather than a competing, readable layer.
  const layers = $derived<Layer[]>(
    stack.map((kind, i) => {
      const depth = stack.length - 1 - i;
      const peek = depth * 26 * DEPTH_MAG;
      const tz = -depth * 200 * DEPTH_MAG;
      const scale = depth === 0 ? 1 : Math.max(0.8, 1 - 0.1 * DEPTH_MAG);
      const bright = depth === 0 ? 1 : 0.22;
      const blurPx = depth === 0 ? 0 : 14 * DEPTH_MAG;
      const opacity = depth === 0 ? 1 : 0.45;

      const closing = kind !== 'hud' && closingOverlay;

      return {
        kind,
        jumpable: depth > 0,
        innerClass:
          kind === 'hud' ? '' : `panel-glass from-${kind}${closing ? ' closing' : ''}`,
        shellStyle:
          `transform: translate(-50%, calc(-50% - ${peek}px)) translateZ(${tz}px) scale(${scale}); ` +
          `filter: brightness(${bright}) blur(${blurPx}px); opacity: ${opacity}; ` +
          `z-index: ${10 + i}; pointer-events: auto;`,
        innerStyle: `pointer-events: ${depth > 0 || closing ? 'none' : 'auto'};`,
      };
    })
  );

  // Settings drops the session while it is open; history does not.
  //
  // Editing an engine, a model or a key mid-session is what the drop is
  // actually for -- those are rebuild-the-session changes, and holding a
  // connection built from stale settings across them is the thing worth
  // avoiding. Reading back a transcript changes nothing, is usually a glance,
  // and a reconnect costs about a second of not being able to talk. Dropping
  // for that would make a glance at the transcript quietly expensive.
  const DROPS_CONNECTION: Record<OverlayKind, boolean> = {
    settings: true,
    history: false,
  };

  function openOverlay(kind: OverlayKind) {
    if (closeTimer) {
      clearTimeout(closeTimer);
      closeTimer = null;
    }
    closingOverlay = false;
    overlay = kind;
    if (DROPS_CONNECTION[kind]) {
      invoke('set_connection_wanted', { wanted: false }).catch((e) =>
        console.error('set_connection_wanted(false) invoke() failed:', e)
      );
    }
  }

  // Plays the sink-into-the-chip animation, then only actually drops the
  // layer from the stack once it has finished.
  function requestCloseOverlay() {
    if (closingOverlay || !overlay) return;
    const closingKind = overlay;
    closingOverlay = true;
    closeTimer = setTimeout(() => {
      overlay = null;
      closingOverlay = false;
      closeTimer = null;
    }, CLOSE_ANIM_MS);
    if (DROPS_CONNECTION[closingKind]) {
      // Asked for immediately rather than after the animation: connecting
      // takes about a second, so starting now means the session is usable
      // roughly when the card finishes sinking away.
      invoke('set_connection_wanted', { wanted: true }).catch((e) =>
        console.error('set_connection_wanted(true) invoke() failed:', e)
      );
    }
  }

  $effect(() => {
    const unlisten = listen('uia://open-settings', () => openOverlay('settings'));
    unlisten.catch((e) => console.error('uia://open-settings listen() failed:', e));
    return () => {
      void unlisten.then((off) => off()).catch(() => {});
      if (closeTimer) clearTimeout(closeTimer);
    };
  });

  // --- FD1 window sizing -------------------------------------------------
  // Resize on every visible-state change. `.center()` is called only on the
  // first resize of an open/close transition — and never once the user has
  // moved the window by hand, which we detect by noticing the window sits
  // somewhere other than where our own last call left it. Centering
  // unconditionally would snap a deliberately-placed overlay back to the
  // middle every time Settings opened.
  let lastAppliedPos: { x: number; y: number } | null = null;
  let userMovedWindow = false;
  let prevOverlayOpen: boolean | null = null;

  async function applyWindowSize(kind: SizeKind, open: boolean) {
    const appWindow = getCurrentWindow();
    try {
      if (!userMovedWindow && lastAppliedPos) {
        const pos = await appWindow.outerPosition();
        if (pos.x !== lastAppliedPos.x || pos.y !== lastAppliedPos.y) {
          userMovedWindow = true;
        }
      }

      const { w, h } = SIZES[kind];
      await appWindow.setSize(new LogicalSize(w, h));

      const firstLayout = prevOverlayOpen === null;
      const openCloseTransition = prevOverlayOpen !== null && prevOverlayOpen !== open;
      if ((firstLayout || openCloseTransition) && !userMovedWindow) {
        await appWindow.center();
      }
      prevOverlayOpen = open;

      const settled = await appWindow.outerPosition();
      lastAppliedPos = { x: settled.x, y: settled.y };
    } catch (e) {
      console.error('window resize failed:', e);
    }
  }

  $effect(() => {
    // Read reactively before the await boundary so the effect tracks them.
    // Settings and history share a footprint, so switching between them is
    // not an open/close transition and must not re-centre the window.
    const current = overlay;
    const small = miniHud;
    const kind: SizeKind = current ?? (small ? 'hudMini' : 'hudFull');
    void applyWindowSize(kind, current !== null);
  });
</script>

<div
  class="deck"
  class:light-mode={lightMode}
  class:glass-wash={glassWash}
  style="--card-radius: {cardRadius}px; --accent: {accent}"
>
  {#each layers as layer (layer.kind)}
    <div class="card-shell" class:jumpable={layer.jumpable} style={layer.shellStyle}>
      {#if layer.jumpable}
        <button
          type="button"
          class="jump-hit"
          aria-label="Back to HUD"
          onclick={requestCloseOverlay}
        ></button>
      {/if}
      <div class="card-inner {layer.innerClass}" style={layer.innerStyle}>
        {#if layer.kind === 'hud'}
          <div class="hud-inner">
            <HudCard
              mini={miniHud}
              onToggleMini={() => (miniHud = !miniHud)}
              onOpenSettings={() => openOverlay('settings')}
              onOpenHistory={() => openOverlay('history')}
            />
          </div>
        {:else if layer.kind === 'history'}
          <div class="panel-inner">
            <HistoryCard onclose={requestCloseOverlay} />
          </div>
        {:else}
          <div class="panel-inner">
            <SettingsCard
              onclose={requestCloseOverlay}
              {uiSettings}
              onvisualchange={(next) => (uiSettings = next)}
            />
          </div>
        {/if}
      </div>
    </div>
  {/each}
</div>

<style>
  /* The overlay window is transparent and undecorated: everything visible is
     the glass itself, so nothing here may paint a page background. */
  :global(html),
  :global(body) {
    margin: 0;
    height: 100%;
    background: transparent;
    overflow: hidden;
  }

  /* Theme tokens ported from the prototype's .stage. --tint is the one lever
     that flips nearly every border/fill/divider between modes; S9 adds the
     light-mode counterpart and wires the accent picker to --accent. */
  .deck {
    position: fixed;
    inset: 0;
    perspective: 1800px;
    font: 400 14px/1.5 -apple-system, 'SF Pro Text', system-ui, sans-serif;
    -webkit-user-select: none;
    user-select: none;

    --accent: #6ea8fe;
    --tint: 255, 255, 255;
    --ink-1: #e8eaf0;
    --ink-2: #cfd3dd;
    --ink-3: #aeb3c0;
    --ink-4: #8890a0;
    --ink-5: #6b7080;
    /* S7: these were a white wash, which is what the prototype's own
       in-page wallpaper needed. Over a real native acrylic backdrop (FD16)
       white tint plus white blur reads as light grey-white plastic, not
       glass. A *dark* translucent tint over the same acrylic is what makes
       it read as smoked glass: the material still comes from the OS, CSS
       just stops bleaching it. The white rim and top highlight stay — they
       are the edge-light that sells the depth — but they belong in the
       borders and inset shadows, not in the fill. */
    --glass-1: rgba(38, 42, 54, 0.52);
    --glass-2: rgba(24, 27, 35, 0.6);
    --glass-3: rgba(14, 16, 22, 0.68);
    --panel-glass-1: rgba(40, 44, 57, 0.66);
    --panel-glass-2: rgba(24, 27, 35, 0.74);
    --panel-glass-3: rgba(13, 15, 20, 0.82);
    /* Fallback only. The real value is set per-platform from the inline
       style on this element (FD17) — this covers the frame between first
       paint and detection resolving, and is the conservative floor so a
       square-cornered window can never flash frosted shoulders. */
    --card-radius: 0px;

    color: var(--ink-1);
  }

  /* S9: the light-mode counterpart of the tokens above. --tint flips to a
     dark rgb triple so borders read as darkening lines against the now-light
     glass instead of white highlights; --accent is not redefined here since
     it is driven live from the inline style above. */
  .deck.light-mode {
    --tint: 20, 22, 30;
    --ink-1: #1b1d24;
    --ink-2: #33363f;
    --ink-3: #4c505c;
    --ink-4: #6b7080;
    --ink-5: #8890a0;
    --glass-1: rgba(255, 255, 255, 0.55);
    --glass-2: rgba(255, 255, 255, 0.22);
    --glass-3: rgba(255, 255, 255, 0.4);
    --panel-glass-1: rgba(255, 255, 255, 0.78);
    --panel-glass-2: rgba(255, 255, 255, 0.42);
    --panel-glass-3: rgba(255, 255, 255, 0.85);
  }

  .card-shell {
    position: absolute;
    left: 50%;
    /* The prototype centred at 42% of a tall design canvas; this window is
       sized to the card itself (FD1), so dead centre is the right anchor. */
    top: 50%;
    transition:
      transform 460ms cubic-bezier(0.19, 1, 0.22, 1),
      filter 460ms ease,
      opacity 320ms ease;
    cursor: default;
  }

  .card-shell.jumpable {
    cursor: pointer;
  }

  /* Click the receded HUD to jump back to it. A real button rather than a
     click handler on the shell, so it is keyboard-reachable for free. */
  .jump-hit {
    position: absolute;
    inset: 0;
    z-index: 1;
    padding: 0;
    border: none;
    background: none;
    cursor: pointer;
  }

  /* FD14/FD16: the window *is* the card. The OS paints the blur across the
     whole window rect and clips it to the window's own rounded corners, so a
     smaller rounded box drawn inside would leave the acrylic showing around
     it as a visible rectangle. The card therefore fills the viewport exactly
     and matches the native corner radius; what CSS still contributes is the
     tint, the rim and the inner highlights — not the shape, and not the
     material. The drop shadow is the OS's too (`shadow: true`), so the
     prototype's `0 24px 70px` outer shadow is gone: with no margin left to
     render into it would only darken the card's own inside edge. */
  .card-inner {
    box-sizing: border-box;
    width: 100vw;
    height: 100vh;
    border-radius: var(--card-radius);
    /* The card is now the window's visible shape, so anything that reaches
       the edge unclipped breaks the illusion — the settings scrollbar was
       squaring off the rounded corners. */
    overflow: hidden;
    background: linear-gradient(160deg, var(--glass-1), var(--glass-2) 55%, var(--glass-3));
    backdrop-filter: blur(30px) saturate(170%);
    -webkit-backdrop-filter: blur(30px) saturate(170%);
    border: 1px solid rgba(var(--tint), 0.16);
    /* S11 "rim" glass-tint mode (the default): an accent-coloured inset rim
       and soft inner glow, layered on top of FD18's white edge-light rather
       than replacing it — the accent reads on the card itself, not just on
       buttons, without a full fill wash fighting the smoked-glass look. */
    box-shadow:
      0 2px 0 rgba(255, 255, 255, 0.14) inset,
      0 -18px 40px rgba(0, 0, 0, 0.18) inset,
      0 0 0 1px color-mix(in srgb, var(--accent) 30%, transparent) inset,
      0 0 16px -3px color-mix(in srgb, var(--accent) 50%, transparent) inset;
  }

  /* S11 "wash" glass-tint mode: additionally blends a low-opacity accent
     gradient into the fill itself, layered above the base glass gradient. */
  .deck.glass-wash .card-inner {
    background-image:
      linear-gradient(160deg, color-mix(in srgb, var(--accent) 22%, transparent), transparent 65%),
      linear-gradient(160deg, var(--glass-1), var(--glass-2) 55%, var(--glass-3));
  }

  /* Settings glass reads much denser than the HUD pill's glass — an almost
     opaque frosted material, not a see-through pane, so the card behind it
     stays legible-but-quiet while this one stays fully readable. */
  .card-inner.panel-glass {
    position: relative;
    background: linear-gradient(
      160deg,
      var(--panel-glass-1),
      var(--panel-glass-2) 45%,
      var(--panel-glass-3)
    );
    backdrop-filter: blur(48px) saturate(150%);
    -webkit-backdrop-filter: blur(48px) saturate(150%);
    border: 1px solid rgba(var(--tint), 0.22);
    /* Genie effect: the panel expands out of the settings cog and, on close,
       shrinks back into that same point — never a plain fade.

       What the eye tracks is where the cog *was at the moment of the click*,
       expressed in the grown window's coordinates. The cog is right-anchored
       (a spacer pushes the four chips to the edge), so its distance from the
       right edge — 85px — is what stays fixed across HUD resizes, and the
       arithmetic is:

         cog in the 342x213 HUD      = (342 - 85, 23)   = (257, 23)
         window grows about centre   = (+129, +185.5)   [(600-342)/2, (584-213)/2]
         cog in the 600x584 panel    = (386, 208.5)     = 64% across, 36% down

       Both halves must be re-derived whenever EITHER box changes. The value
       this replaced (66%/38%) was left behind by two such changes: it showed
       its working against a 424px-wide settings panel that had since grown
       to 600, which had put the horizontal ~5% out. The prototype's 80%/42%
       was tuned to its own canvas and never applied here.

       No chip was added to that row, so none of this moved: history opens
       from the recent-messages strip above the text input instead. */
    animation: panelOpenFromCog 420ms cubic-bezier(0.34, 1.4, 0.4, 1) both;
  }

  .card-inner.panel-glass.from-settings {
    transform-origin: 64% 36%;
  }

  /* History grows out of the log strip it was opened from, by the same
     arithmetic against a different point. The strip is flex-sized rather
     than at a fixed offset, so this is derived from the laid-out box and is
     approximate in a way the cog's origin is not:

       slot spans y 42..165   [12 top pad + 22 top row + 8 gap; 28 controls,
                               8 gap and 12 bottom pad off the other end]
       strip is the slot less the ~30px input row and the 4px gap
                              = y 42..131, centre ~86
       strip spans x 104..328 [14 pad + 80 avatar + 10 gap, to 342 - 14]
                              = centre 216
       window grows about centre = (+129, +185.5)
       strip in the 600x584 panel = (345, 272) = 58% across, 47% down

     The ±6px of slack in the input row's height moves this by well under a
     percent, so it does not need to be exact to read as growing from the
     right place. */
  .card-inner.panel-glass.from-history {
    transform-origin: 58% 47%;
  }

  .card-inner.panel-glass.closing {
    animation: panelSuckIntoCog 360ms cubic-bezier(0.5, 0, 0.85, 0.35) both;
  }

  /* S11 "wash" glass-tint mode, settings panel variant — same accent
     gradient layered over the denser panel-glass fill. */
  .deck.glass-wash .card-inner.panel-glass {
    background-image:
      linear-gradient(160deg, color-mix(in srgb, var(--accent) 16%, transparent), transparent 65%),
      linear-gradient(160deg, var(--panel-glass-1), var(--panel-glass-2) 45%, var(--panel-glass-3));
  }

  @keyframes panelOpenFromCog {
    0% {
      opacity: 0;
      transform: scale(0.04);
      filter: blur(6px);
    }
    45% {
      opacity: 1;
    }
    70% {
      transform: scale(1.05);
      filter: blur(0);
    }
    100% {
      opacity: 1;
      transform: scale(1);
      filter: blur(0);
    }
  }

  @keyframes panelSuckIntoCog {
    0% {
      opacity: 1;
      transform: scale(1);
      filter: blur(0);
    }
    35% {
      transform: scale(1.05);
      filter: blur(0);
    }
    100% {
      opacity: 0;
      transform: scale(0.04);
      filter: blur(6px);
    }
  }

  /* HudCard owns its own padding and fills this box exactly, so the deck
     contributes position and size only — a grid that centred its child
     would shrink-wrap the card's own flex layout. */
  .hud-inner {
    position: relative;
    box-sizing: border-box;
    width: 100%;
    height: 100%;
    display: flex;
  }

  /* Fixed footprint so switching tabs never resizes the window; content that
     overflows scrolls inside instead. */
  .panel-inner {
    position: relative;
    box-sizing: border-box;
    width: 100%;
    height: 100%;
    display: flex;
    flex-direction: column;
  }

  /* FD7: there is no animations toggle — prefers-reduced-motion is respected
     as a baseline accessibility default, not as a user-facing setting. */
  @media (prefers-reduced-motion: reduce) {
    .card-shell,
    .card-inner,
    .card-inner.panel-glass,
    .card-inner.panel-glass.closing {
      animation: none !important;
      transition: none !important;
    }
  }
</style>
