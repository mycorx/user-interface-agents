# Live audio device switching

Date: 2026-10-31. Status: draft for approval.

## Intent

The user keeps several audio devices connected at once (laptop speakers,
a headset, earbuds) and moves between them through the day. The input and
the output are often from different devices, for example speakers for
output and a headset microphone for input.

Today any change on the Audio tab needs a restart. The app opens the
microphone and the speaker once, at launch, in `build_session`, and keeps
them for the life of the process. The goal is that changing devices never
needs a restart, on macOS, Windows and Ubuntu.

## Behaviour

1. **Picking in Settings applies on close.** The user picks the input and
   output separately on the Audio tab and saves. When Settings closes, the
   session reconnects as it already does, using the new devices. No
   restart. Echo cancellation (`aec_enabled`, Windows and macOS only) and
   barge-in sensitivity apply the same way, because they sit on the same
   tab.
2. **"Default" follows the OS at any time.** A direction set to "Default"
   uses whatever the OS uses right now. On Windows that is the default
   communication device (`eCommunications`), with echo cancellation on or
   off (user). It is not the ordinary default device. That is the endpoint
   the Communications path already uses and is known to cancel echo on. When the OS default changes (in
   macOS Sound settings, Windows Sound settings or the Ubuntu sound panel),
   UIA moves to it within about two seconds. That includes mid-conversation,
   without disconnecting from the speech engine. A short gap in audio during
   the move is acceptable. Audio already queued for playback carries on from
   the new speaker where the backend allows it.
3. **A pinned device stays pinned.** A direction set to a named device
   ignores OS default changes.
4. **A pinned device that is missing is waited for.** This holds when a
   pinned device goes away mid-conversation (battery dies, cable pulled),
   when it is missing at launch, and when it is missing as Settings closes
   (user). UIA does not fall back to another device. It keeps retrying with
   backoff, shows that it is waiting, and resumes when the device appears.
   The other direction keeps working meanwhile.
5. **No restart notice for Audio.** The Audio tab stops showing "Restart
   the assistant". Providers, MCP and General still do.
6. **The user picks; UIA never guesses.** UIA does not switch on its own
   when a device is plugged in. It moves only when the user picks, or when
   the OS default moves while the direction is "Default".

## Goals

- Settings changes to input, output, AEC and barge-in apply without a
  restart, on all three platforms.
- "Default" follows the OS default live, on all three platforms.
- Pinned devices are waited for, never silently replaced.
- The behaviour is the same on every platform, and tests cover it without
  real hardware.

## Non-goals

- A quick device picker on the HUD. Settings is good enough (user).
- Switching automatically when a new device is plugged in.
- Live switching of engine, model or MCP settings. They keep the restart
  notice.
- Linux setups other than the latest Ubuntu (PipeWire). Plain ALSA and
  PulseAudio-only systems are out of scope.
- Echo cancellation on Linux (user). The Audio tab's echo cancellation
  toggle is disabled on Linux, with a short note saying it is not available
  there. `aec_enabled` stays ignored on Linux.
- The engine picker's launch-snapshot display (`get_engine`). Separate
  follow-up.

## Anti-goals

These would count as failure even if switching technically works:

- Falling back to another device when a pinned one disappears.
- Disconnecting the engine, or losing the conversation, when only the OS
  default moved.
- Running two echo-cancellation paths at once (OS AEC under AEC3), or
  pairing a cancelling capture with a non-cancelling render. The existing
  rule "either half failing takes both down" stays.
- Opening a new macOS Voice Processing IO unit while the old one still
  exists.
- A device failure that leaves the HUD looking healthy while no audio flows.

## Current state, per platform

- **Session (`uia-core`).** `SessionDeps` owns `source` and `sink` as boxed
  trait objects, for the whole session. `Session::run` parks while no
  connection is wanted (Settings is open), reconnects when one is wanted
  again, and already applies a queued prompt swap (`SessionControl::
  request_prompt_swap`) at a safe point in the pump loop.
  `set_barge_in_threshold` exists, but only the shell calls it, at build
  time.
- **The run loop stalls on a silent source.** `Session::run` awaits
  `source.next_frame()` once per pass. Every backend simply stops yielding
  frames while its device is gone, without returning `None` and without
  sending silence. So while a mic is missing, the loop drains no engine
  events (no audio is heard, even from a working speaker), never rotates the
  session, and never re-reads `wanted`. Opening Settings then never parks
  the session.
- **App (`uia-app/src/session.rs`).**
  - On Windows and macOS, `os_aec_or_fallback_cpal` picks the pair: the OS
    echo-cancelling pair (WASAPI Communications or VPIO) when `aec_enabled`
    is on, otherwise plain cpal devices.
  - On Linux the pair is opened inline in `build_session`, with plain cpal,
    optionally wrapped in AEC3 behind the `aec` feature. No release build
    enables that feature, so `aec_enabled` does nothing on Linux today.
  - With `aec`, an echo-canceller start-up failure reopens the default
    devices, which drops pinned names. That breaks the "never fall back"
    anti-goal and must go.
  - At startup, `resolve_audio_settings` replaces a persisted pinned device
    that is not plugged in with the `uia.toml` `[audio]` device, or with the
    OS default when there is none. It checks the name against the input and
    output lists merged together.
- **macOS (`uia-audio/src/coreaudio`).** A supervisor thread owns one VPIO
  unit for both directions. It listens to CoreAudio for the default
  devices, the device list and the bound devices. It rebuilds the unit when
  a bound device dies, or when an unpinned direction's default moves, and
  backs off while a device is gone. Pinned names are resolved only between
  units. Behaviours 2–4 already hold on this path. What it lacks is a way
  to change `Wanted` while running.
- **Windows (`uia-audio/src/wasapi`).** Independent capture and render
  worker threads open the `eCommunications` endpoint, or the first device
  matching a pinned name. They reopen with backoff when the client is
  invalidated, re-resolving the name on every attempt, so a pinned device
  is already waited for (behaviour 4). They do not react to a default
  change: there is no `IMMNotificationClient`. With AEC off, or when the OS
  path fails, Windows opens plain cpal devices instead. cpal 0.18's WASAPI
  host does not reroute on a default change: it reports `StreamInvalidated`
  or `DeviceNotAvailable`, and our `CpalSource`/`CpalSink` then stop for
  good, because their error callback only prints. cpal follows `eConsole`,
  whereas the Communications path follows `eCommunications`.
- **Linux (`uia-audio/src/input.rs`, `output.rs`).** cpal's default host
  is ALSA, because the `pipewire` and `pulseaudio` features are off. There
  is no reopen: an error ends the stream for good. The same is true of the
  cpal path on every OS. cpal 0.18's `pipewire`
  host reroutes default-built streams when the default changes and reports
  `ErrorKind::DeviceChanged`.

## Decided

- **The switch happens when Settings closes,** at the reconnect that
  already happens there (user: Settings is good enough).
- **Waiting for a pinned device, no fallback** (user).
- **Linux target: the latest Ubuntu only** (user). *My call:* enable cpal's
  `pipewire` feature on Linux, and add `libpipewire-0.3-dev` to the Linux CI
  and release jobs that install `libasound2-dev`.
- **Two mechanisms, one per trigger** (my call):
  - *Settings changes* swap the whole audio pair at the session level:
    drop the old pair first, then open the new one with the same opener
    startup uses. This covers an AEC toggle too, which changes the backend
    type (VPIO and cpal on macOS, WASAPI Communications and cpal on
    Windows), so a change inside one backend cannot express it.
  - *OS default changes* are followed inside each backend, as macOS
    already does, so the engine connection and the session are never
    involved.
- **Drop before open** for the pair swap (my call). It is required for VPIO
  and harmless elsewhere. The cost is a short window with no devices, which
  falls while Settings is open and the session is parked.
- **The session never stalls on the microphone** (my call). Capture is
  decoupled from the pump loop, for example as its own task feeding a
  channel that the loop `select!`s on alongside engine events and the
  `wanted` watch. The loop doc already names this as the fix. Then, while a
  pinned mic is being waited for:
  - the session stays connected and keeps playing through a working
    speaker;
  - it still parks when Settings opens, so the user can always switch away
    from a dead pinned device;
  - rotation and the idle deadline keep working. With no mic, the idle
    deadline disconnects as usual.
- **A cpal reopen loop on every OS** (my call), not only Linux. Windows and
  macOS use cpal whenever AEC is off. On a lost device or a default change
  it reopens with backoff, re-resolving a pinned name on each attempt. On
  PipeWire, a reroute (`DeviceChanged`) needs no reopen.
- **Waiting is visible** (my call). While a direction waits for its device,
  the HUD shows it through the existing state or error channel, naming the
  device. Exact wording is the builder's.
- **The opener is handed to the session by the shell** (my call). `uia-core`
  stays free of device code: the session gets a factory, or a requested
  replacement pair built by the shell, through `SessionControl`, mirroring
  the prompt swap. The barge-in threshold travels with the request.

## Left to the builder

- **The shape of the swap API.** A factory closure in the request, or the
  shell opening the pair itself after the session confirms it dropped the
  old one. It has to honour drop-before-open, and it should take effect
  while the session is parked (Settings open), before the reconnect. A swap
  requested while connected (not expected from the UI) may apply at the
  next safe point in the pump loop, like the prompt swap.
- **Windows default following.** Register an `IMMNotificationClient` per
  worker, or one shared. Queued render audio should survive the reopen, as
  it does on invalidation today.
- **The cpal reopen loop's shape.** The existing `backoff::reopen_delay_ms`
  is the model. cpal's macOS host already reroutes default output streams.
  Confirm input does too.
- **How capture is decoupled from the pump loop,** keeping the existing
  ordering guarantees around reconnect and barge-in.
- **Tests.** Fake sources and sinks for the swap in `uia-core`. Pure-logic
  tests for the default-following and pinned-waiting decisions, in the
  style of `direction_stale`. Real-device checks stay manual (see below).
- **Settings UI.** Remove Audio's restart trigger. The Save behaviour stays
  as it is.

## Consequences of the user's answers

- **Waiting replaces every first-open failure for a pinned name.** Every
  backend treats its first open as fatal today: WASAPI `capture_run` and
  `render_run`, CoreAudio `supervise`, and cpal `input`/`output` on a name
  miss. Each must instead be able to start in a waiting state, report that,
  and keep retrying. On macOS, where VPIO serves both directions, a missing
  pinned direction holds the whole unit, so the other direction is opened
  as plain cpal until the unit can be built. A failure that is not a
  missing pinned name (no default device at all, or an OS error) keeps
  today's handling.
- **Startup stops dropping missing pinned devices.** `resolve_audio_settings`
  no longer filters by the devices present. The device filter and its
  merged input-and-output name check go. The `uia.toml` `[audio]`
  precedence stays.
- **The AEC fallback path keeps the pins.** Opening the OS echo-cancelling
  pair and falling back to cpal both use the pinned names. Neither may
  substitute the default for a pinned name. That includes removing
  `wrap_with_aec_if_rates_match`'s reopen of the default devices.
- **Windows follows `eCommunications` on both paths.**
  - The Communications path keeps its endpoint. It gains a reopen when
    `OnDefaultDeviceChanged` fires for the matching flow with the
    `eCommunications` role, for an unpinned direction only.
  - The plain path (AEC off, or the OS path failed) must also open and
    follow the communications default, not cpal's `eConsole` default. How
    is left to the builder. Options include resolving the
    `eCommunications` endpoint and opening that device through cpal by
    id, or running our WASAPI workers without the Communications category.
  - The Audio tab's "Default" hint on Windows should say it follows the
    default communication device.

## Verification

- Unit tests as above, on every OS in CI. Clippy and fmt clean. Run the
  repository checks `build-macos-package.sh` runs.
- A manual checklist added under `docs/`, run on a real Mac, a Windows PC
  and Ubuntu:
  - Switch output from speakers to a headset in Settings mid-conversation:
    the assistant is heard on the headset after Settings closes.
  - Mixed pair: speaker output with headset microphone input.
  - With "Default", change the OS default during a reply: the audio moves,
    and the conversation stays connected.
  - With a pinned headset, change the OS default: nothing moves.
  - Turn the pinned headset off mid-conversation: UIA waits and does not
    switch. Turn it back on: audio resumes.
  - Toggle echo cancellation in Settings: it applies with no restart and no
    echo.
- Before merging: an Aikido scan, and the live voice check the webrtc pins
  require.
