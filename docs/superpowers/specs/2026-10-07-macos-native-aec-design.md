# macOS-native echo cancellation, via Voice Processing IO

## Problem

On macOS the assistant hears itself. Windows has had OS-level echo
cancellation on by default since S17: `uia-audio::wasapi` opens capture
and render under WASAPI's `AudioCategory_Communications`, Windows applies
its own AEC/AGC/NS beneath the app, and `uia-app` wires it in through a
`[target.'cfg(windows)'.dependencies]` stanza so nobody has to remember a
flag. macOS gets plain cpal devices and no echo cancellation at all
unless someone builds with `--features aec` — the bundled-C++ AEC3 path
that needs meson, ninja and libclang on every build machine, and that
nothing turns on by default.

macOS has the direct equivalent of the Communications category: the
**Voice Processing IO** audio unit (`kAudioUnitSubType_VoiceProcessingIO`,
"VPIO"). Audio played through its output element is the far-end
reference; audio read from its input element has that reference
cancelled out, plus AGC and noise suppression. No bundled DSP, no
toolchain — the same trade the Windows path made.

The goal is parity with Windows, including device selection: the same
`[audio]` config (`aec_enabled`, `input_device`, `output_device`) and the
same Settings → Audio controls must behave the same way on both
platforms.

## What the spike established (2026-10-07)

A throwaway Swift CLI (session scratchpad, not committed) drove a raw
VPIO unit on macOS 27.0.1 with real devices. Swift because this Mac had
no Rust toolchain at the time; the questions were about OS behaviour, not
the language.

| Question | Result |
|---|---|
| Can input and output be bound to **different, user-named** devices? | **Yes.** `kAudioOutputUnitProperty_CurrentDevice` set on element 1 (input) and element 0 (output) independently; read back after start, both honoured. Verified with Jabra Evolve2 30 SE + Creative Pebble X, C920 webcam + Pebble X, MacBook mic + MacBook speakers. |
| Does it cancel our own playback? | **Yes.** C920 + Pebble: raw echo −44.9 dBFS (11 dB over its floor) → −70.3 dBFS with VPIO on, 3 dB *below* the processed floor. All measurable echo removed; spread between repeat phases ≤ 0.7 dB. The ceiling at loud volumes was not measured. |
| Does it suppress the user's voice while cancelling (would barge-in break)? | **No.** Double-talk: voice kept during playback −0.5 dB (within the 3–4 dB natural variation between talk phases); voice sits 49.6 dB above residual echo. |
| Do device rates change when VPIO opens? | **No.** Nominal rates identical before and after. |
| Can other-app ducking be minimised? | **Yes.** `kAUVoiceIOProperty_OtherAudioDuckingConfiguration` (macOS 14+) accepted with `.min`. |
| Client format? | 48 kHz mono f32 accepted on both elements; VPIO converts to the hardware rate. |

Findings that shape the design:

- **VPIO processes at the lower device rate.** With the 16 kHz C920, the
  speaker side ran at 16 kHz too: the assistant's voice loses its top
  end on that pairing. The app is unaffected (client format stays 48 kHz)
  but the backend logs it.
- **Callback period follows that rate**: ~10.7 ms at 48 kHz, ~36 ms at
  16 kHz. Both are inside the 50 ms barge-in budget; `clear()` must not
  depend on device buffering.
- **A dead microphone is silent, not an error.** With the lid closed the
  built-in mic delivered exact zeros while every call returned `noErr`.
  No error-driven reopen can see this; out of scope (cpal behaves the
  same today).

## Approach

**Raw VPIO AudioUnit through the C API**, via `objc2-audio-toolbox`,
`objc2-core-audio` and `objc2-core-audio-types` — the exact crates (0.3.2)
cpal already pulls in on macOS, so no new third-party dependencies, only
extra features on existing ones. No Objective-C runtime involved.

Rejected:

- **AVAudioEngine + `setVoiceProcessingEnabled(true)`**: less code to
  first sound, but device selection on macOS means reaching into the
  underlying unit anyway, configuration changes arrive as notifications,
  and there is less control over latency and `clear()`.
- **Turning on AEC3 (`aec`) by default on macOS**: not OS-native,
  requires the C++ toolchain on every build machine, and is not what
  "the same as Windows" means.

The two echo-cancellation paths remain alternatives, never composed:
AEC3 over audio VPIO already cancelled is filtering twice.

## Design

### 1. Module layout and feature gating

- New `uia-audio` feature **`coreaudio-aec`**, enabling the objc2 audio
  crates from a `[target.'cfg(target_os = "macos")'.dependencies]` table.
  A no-op elsewhere, like `wasapi-aec`.
- New module `uia-audio/src/coreaudio/`, guarded in `lib.rs` by
  `#[cfg(all(target_os = "macos", feature = "coreaudio-aec"))]`:

  | File | Purpose |
  |---|---|
  | `mod.rs` | module docs; `open_voice_processing`; supervisor thread and shutdown |
  | `device.rs` | resolve default/named `AudioDeviceID`s, device names, liveness and default-device listeners |
  | `unit.rs` | create and configure the VPIO unit, bind devices, set client formats, callbacks, start/stop/dispose |
  | `capture.rs` | `CoreAudioSource: AudioSource` |
  | `render.rs` | `CoreAudioSink: AudioSink` |

- Public entry point:
  `coreaudio::open_voice_processing(input: Option<&str>, output: Option<&str>) -> Result<(CoreAudioSource, CoreAudioSink), AudioError>`.
  One call, not two, because one unit serves both directions; `uia-app`
  always used the WASAPI pair together anyway.
- `REOPEN_DELAYS_MS` and `reopen_delay_ms` move from `wasapi/mod.rs` to
  a new platform-neutral `uia-audio/src/backoff.rs`, with their tests.
  `wait_before_reopen` stays in `wasapi` (it waits on a Win32 event).
  This pure move is the only change to Windows code in `uia-audio`.
  `LiveFormat` stays in `wasapi`: the macOS format never changes (§2).

### 2. Shared-unit lifecycle

**Ownership.** `open_voice_processing` starts one supervisor thread,
`uia-coreaudio-vpio`, which owns the VPIO unit for the backend's whole
life. `CoreAudioSource` and `CoreAudioSink` each hold an `Arc` to shared
state: the playback queue, the capture channel sender, the stop flag and
the supervisor's wake signal. When the last of the two is dropped, the
supervisor is stopped and joined, and the unit stopped and disposed.

**Open errors.** First-open failure is returned synchronously through a
ready-handshake, as `WasapiSource`/`WasapiSink` do. A configured name
that matches no device is `AudioError::DeviceUnavailable` containing
"no audio device matching" — never a silent fallback to the default —
and `uia-app` then falls back to cpal (§3).

**Format.** Client side is always 48 kHz mono on both elements; VPIO
converts to and from the hardware. `format()` is therefore constant
(48 000 Hz, 1 channel) and survives reopens unchanged. The session reads
only `sample_rate_hz` from source and sink formats, so reporting mono is
safe. The opened-device log line names both devices and the rate VPIO
actually runs at (the lower device rate), so a 16 kHz pairing is visible.

**Capture.** The input callback calls `AudioUnitRender` into a
preallocated buffer, converts f32 → i16 with clamping, and `try_send`s
into a bounded `mpsc(64)`: when the consumer is behind, the newest frame
is dropped. Same policy as `WasapiSource`/`CpalSource`; the real-time
thread never blocks.

**Playback and `clear()`.** The same `Arc<Mutex<VecDeque<i16>>>` queue
as `WasapiSink`/`CpalSink`: `write` appends, `clear()` empties it
synchronously. The render callback takes the lock with `try_lock`; on
contention or underrun it outputs silence. After `clear()`, at most one
callback period of already-committed audio plays (~11 ms at 48 kHz,
~36 ms with a 16 kHz device), inside the 50 ms budget.

**Reopen.** The supervisor registers property listeners for
`kAudioDevicePropertyDeviceIsAlive` on both bound devices and
`kAudioHardwarePropertyDevices` on the system object. Listeners only
signal the supervisor; all rebuilding happens on the supervisor thread.
On loss it disposes the unit, waits with the shared backoff
(200 ms → 5 s, retrying forever, cut short by shutdown), re-resolves
both devices by the configured names, and builds a new unit. The
playback queue and capture channel live in the shared state, so they
survive a reopen.

**Following the system default.** When `input_device`/`output_device` is
unset, the supervisor also listens to
`kAudioHardwarePropertyDefaultInputDevice`/`DefaultOutputDevice` and
rebuilds the unit, through the same path as a reopen, when the default
changes. A configured name stays pinned and ignores default changes.
This deliberately differs from Windows, where an opened stream stays on
its endpoint until it disappears: macOS users switch default output
often (AirPods, docks), VPIO does not follow on its own, and "no device
configured" should mean "whatever the OS says is current" on both
platforms.

### 3. `uia-app` wiring

**Cargo.** A `[target.'cfg(target_os = "macos")'.dependencies]` stanza
in `crates/uia-app/Cargo.toml` enables
`uia-audio = { ..., features = ["coreaudio-aec"] }`, beside the Windows
stanza. macOS gets it by default — including `pnpm tauri dev` and
`tauri build`, whose `build.features` cannot vary per platform. Linux
resolution is untouched.

**Composition in `session.rs`.**

- `should_use_wasapi_communications` → `should_use_os_echo_cancellation`;
  `wasapi_or_fallback_cpal` → `os_aec_or_fallback_cpal`, compiled for
  `any(windows, target_os = "macos")`.
- A per-OS `open_os_aec(config) -> Result<AudioPair, AudioError>`:
  WASAPI's two constructors on Windows, `open_voice_processing` on macOS.
- Fallback behaviour is unchanged and shared: `aec_enabled = false` opens
  plain cpal devices with the same configured names; an open failure
  falls back to cpal and logs that echo cancellation is off. Log wording
  becomes platform-neutral and names the backend.
- The AEC3 branch's guard changes from `#[cfg(not(windows))]` to
  `#[cfg(not(any(windows, target_os = "macos")))]`, so `--features aec`
  on macOS can never stack AEC3 over VPIO.
- The two existing `aec_enabled` composition tests are renamed and keep
  running on every platform.

**Config and UI text.** `AudioSection` is unchanged. Its `aec_enabled`
doc comment, `uia.example.toml`'s `[audio]` comment, and the Echo
cancellation and device help text in `src/SettingsAudio.svelte`
(currently "Windows' own…", "no effect on Linux/macOS") become
platform-neutral: OS echo cancellation on Windows and macOS; the `aec`
feature remains the Linux option. The device dropdowns already list
cpal's CoreAudio names, which are what `device.rs` matches with
`device_name_matches`; no UI logic changes.

**Build-resolution check.** `scripts/check-windows-wasapi-default.sh`
becomes `scripts/check-os-aec-defaults.sh`, asserting via
`cargo tree --target` that `x86_64-pc-windows-msvc` has `wasapi-aec`,
`aarch64-apple-darwin` and `x86_64-apple-darwin` have `coreaudio-aec`,
and `x86_64-unknown-linux-gnu` has neither. It runs on any host. Today
nothing invokes the old script — only the comment in
`crates/uia-app/Cargo.toml` names it — so this adds it as a step in
`.github/workflows/ci.yaml` (Ubuntu, `cargo tree` only, no toolchain
beyond cargo) and updates that comment.

### 4. Testing

**Unit tests** (no device). Platform-neutral ones run in Ubuntu CI;
macOS-only ones run with `cargo test` on a Mac.

- `backoff.rs`: the moved saturation and increasing-delay tests.
- Pure functions in `coreaudio/`, written so they need no device:
  f32 → i16 conversion clamps rather than wraps; queue draining pads with
  silence on underrun and on `try_lock` failure; device resolution applies
  `device_name_matches` over a fake device list; the rebuild decision
  (bound device gone → rebuild; default changed and no name configured →
  rebuild; default changed with a pinned name → no rebuild).
- `CoreAudioSource` and `CoreAudioSink` are `Send`.

**Hardware tests** (`#[ignore]`, run manually on a Mac, mirroring the
WASAPI ones): default devices open, `format()` is 48 kHz mono and the
source yields frames; `clear()` empties synchronously; an unmatched name
is the "no audio device matching" error; a split pair of named devices
opens with both bindings honoured; the observed render-callback period
on the default devices is ≤ 50 ms. That last one replaces WASAPI's
buffer-size unit test: VPIO chooses its own period, so there is no
requested size to assert on, only what it actually does.

**Manual test.** A macOS block in `docs/MANUAL-TEST.md`, beside the
Windows one:

- the startup log shows VPIO opened on the expected devices (seeing the
  cpal fallback warning instead means the rest fails by design);
- speakers at volume, the assistant does not interrupt itself;
- talking over the assistant interrupts it promptly;
- unplugging and replugging mic or speakers recovers without a restart;
- with no device configured, switching the system output follows it;
- another app's music is only minimally ducked;
- `aec_enabled = false` gives plain devices.

## Constraints and prerequisites

- **No macOS CI runner.** Every CI job runs on Ubuntu. The macOS code is
  compile-checked and tested locally on a Mac; CI only verifies feature
  resolution through the check script.
- **Toolchain.** Rust 1.99 is installed on the development Mac at
  `~/.cargo/bin` (workspace MSRV 1.85, edition 2024).
- **Minimum macOS.** Ducking configuration needs macOS 14; on older
  systems it is skipped with a log line rather than failing the open.

## Out of scope

- Detecting a microphone that delivers silence without erroring (closed
  lid, hardware mute).
- Measuring VPIO's echo-cancellation ceiling at high playback volume.
- iOS. VPIO exists there too, but the session setup (`AVAudioSession`)
  is a separate piece of work.
- Changing Windows behaviour beyond the `backoff.rs` move and the
  shared, renamed composition code.
