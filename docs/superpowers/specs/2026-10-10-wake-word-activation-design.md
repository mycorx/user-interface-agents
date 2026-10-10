# Wake-word activation (configurable assistant name)

Status: **Proposed**, not started. Written 2026-10-10.

## Problem

Today the user starts talking by unmuting the mic in the HUD, and the
global shortcut (`CommandOrControl+Shift+Space`) only shows or hides the
window. Nothing is hands-free. Leaving the mic unmuted instead streams
every frame to the provider, which costs money (uncached audio input is
$32/1M tokens on OpenAI) and sends room audio off the device.

The feature: the assistant stays muted until it hears its name, in the
style of "Hey Siri" or "Alexa", where the name comes from configuration.
After the wake word it listens, replies, allows a short follow-up, and
goes back to sleep.

## Recommendation

Build it, opt-in and off by default, starting with a 2–3 day spike with
a go/no-go gate. The integration into this codebase is small. The hard
part is detecting an arbitrary configured name reliably, which needs an
open-vocabulary keyword spotter rather than a per-word trained model.

Estimated effort: **2.5–4 weeks for one developer on desktop**, plus
mobile work listed separately below.

## Why it fits the current architecture

1. **The mic is already read while muted.** `Session::pump_audio`
   (`crates/uia-core/src/session/mod.rs`, the `capture_enabled()` check)
   reads every frame and drops it when capture is off. A detector takes
   those dropped frames; on a match it calls `set_capture_enabled(true)`,
   the same call the HUD mute button makes. The state machine in
   `session/state.rs` does not change.
2. **The session stays connected while muted.** The idle deadline is off
   (`crates/uia-app/src/session.rs`, `set_idle_deadline(Duration::ZERO)`),
   so waking only unmutes. There is no reconnect, so the reply is
   immediate.
3. **There is already a seam for it.** The `Activation` trait
   (`crates/uia-core/src/activation/mod.rs`) exists to express "the user
   wants to talk now". A wake-word detector is another source of that.
4. **Echo cancellation is already on** (WASAPI Communications on Windows,
   Voice Processing IO on macOS), which reduces the assistant waking
   itself by saying its own name.
5. **Personas already have a spoken name** (`crates/uia-app/src/personas.rs`).
   The wake word can default to the active persona's spoken name, so a
   persona switch also changes the wake word.

## Detection approach

Most wake-word engines need a model trained per phrase, which rules out
"whatever name we configure".

| Option | Arbitrary name at runtime? | Licence / cost | Verdict |
|---|---|---|---|
| sherpa-onnx keyword spotter | Yes, keyword given as text, no training | Apache-2.0 (confirm the specific model's licence) | Recommended baseline |
| Porcupine (Picovoice) | No: one console-trained file per word and platform | Commercial licence and access key | Does not meet the requirement |
| openWakeWord | No: training run per word | Pretrained models are CC BY-NC-SA | Does not meet the requirement; licence conflicts |
| Local streaming speech-to-text plus text match | Yes | Usually fine | Heavier on CPU, slower, more false triggers |
| Ask the cloud model to reply only when addressed | Yes | Streams and bills all audio | No: cost, privacy, unreliable |

### Our own crate, not our own model

Create a new crate, `uia-wake`, regardless of the engine inside it. It
owns:

- the detector trait (the engine sits behind it and can be swapped);
- a ring buffer of about 1.5 s of audio from before the wake;
- the follow-up window and re-mute logic;
- thresholds and config handling;
- audio features (mel filterbank from raw PCM, a few hundred lines of
  Rust), with resampling to 16 kHz through the existing resampler.

Do **not** train our own detection model. That is months of
machine-learning work (data, training, false-trigger measurement), and
it is where accuracy comes from. Use an existing pre-trained
open-vocabulary model.

### Engine inside `uia-wake`

| Engine | Build | Mobile | Extra effort |
|---|---|---|---|
| sherpa-onnx (C++, via Rust bindings) | Needs onnxruntime binaries per platform; MSVC risk | Separate prebuilt iOS and Android libraries to manage | Baseline |
| tract (pure-Rust ONNX runtime from Sonos, built for on-device wake words) or candle | `cargo build` only, no C++ toolchain | Cross-compiles to iOS and Android like the rest of the app | +1–2 weeks |

With the pure-Rust engine we write the decoding step ourselves: scoring
whether the audio matches the configured name against a small CTC or
transducer speech model, and turning the name into the model's tokens.
A model that works on letters or subword pieces avoids needing a
pronunciation dictionary.

The repo's history with `webrtc-audio-processing` (it would not build
under MSVC) and the mobile plans both favour pure Rust. The main unknown
is whether tract or candle supports every operation the chosen model
uses; check that on day one of the spike.

Plan: run both engines in the spike with the same model. If tract
matches sherpa-onnx's accuracy, ship pure Rust. Otherwise ship
sherpa-onnx behind the same trait and revisit later.

## Behaviour

- **Wake:** a detection while muted sets capture on and sends the
  buffered pre-wake audio first, so "Hey X, what's on today" said in one
  breath is not cut off. Leaving the wake word in the audio is simpler
  than trimming it, and the model handles it.
- **Follow-up window:** after the assistant finishes speaking, stay
  unmuted for a configurable few seconds, then re-mute and return to
  listening for the wake word.
- **While the assistant speaks:** ignore detections. Interrupting by
  talking over it already works through the barge-in level check.
- **Mute button wins:** a manual mute turns wake-word listening off
  until the user turns it back on. The HUD shows a distinct
  "listening for *Name*" state.
- **Name guidance:** short names ("UIA", "Max") false-trigger far more
  than 3–4 syllable phrases ("Hey Juniper"). Settings should say so.
  Don't use "Siri" or "Alexa": they are trademarks and would wake real
  devices nearby.

## Configuration (proposed)

```toml
[activation]
# Off by default: the mic is listened to locally all the time when on.
wake_word_enabled = false
# Empty means the active persona's spoken name.
wake_word = ""
# 0.0-1.0. Higher wakes more easily and false-triggers more.
wake_sensitivity = 0.5
# Seconds to stay unmuted after the assistant stops speaking.
follow_up_secs = 8
```

All four also appear in Settings.

## Risks

- **Accuracy varies by name and mic.** Needs per-name tuning and a
  sensitivity control.
- **Native build** if sherpa-onnx is chosen: prove the Windows/MSVC build
  first, using prebuilt binaries.
- **Privacy.** Always listening, though only on the device. Opt-in, a
  clear HUD state, and the mute button as master control.
- **Packaging.** Model files add several MB to the installer (estimate;
  measure in the spike). Their licences go into `NOTICE.txt`.
- **CPU and battery.** Continuous inference is small on desktop but needs
  measuring, and matters more on mobile.

## Mobile

The wake-word code carries over, especially in pure Rust: the Rust core,
`uia-audio`, and `uia-wake` all run on iOS and Android. What does not
carry over is listening in the background. Both operating systems
restrict it on purpose, and only their own assistants get the low-power,
always-on wake-word hardware.

### iOS

- **App open:** our own wake word works normally.
- **In the background:** technically possible with the `audio`
  background mode if listening started while the app was open. However,
  App Review tends to reject always-listening apps, the orange mic
  indicator stays on, and it drains the battery. Do not rely on it.
- **What to build instead:** an App Intents shortcut so the user says
  "Hey Siri, talk to UIA". The phrase must include the app's name, so a
  custom wake name cannot be used here. Add the iPhone Action button and
  a Control Center control as one-tap starts.

### Android

- **App open:** our own wake word works normally.
- **"Hands-free mode":** a foreground service of type `microphone` with a
  persistent notification. Recent Android versions only allow starting it
  while the app is visible, and some manufacturers' battery savers kill
  it. Fine as an explicit opt-in, for example while driving.
- **Built-in wake words** use a privileged permission
  (`CAPTURE_AUDIO_HOTWORD`) and low-power hardware that ordinary apps
  cannot use. An app can become the **default digital assistant**, which
  gives it the system assist gesture (long-press power or home), but not
  a custom wake word.

### Native plugins

The background parts need small native Tauri plugins: a Kotlin
foreground service on Android, and Swift for App Intents and the audio
session on iOS.

### Product position

- **Desktop:** full custom wake word, opt-in.
- **Mobile:** custom wake word while the app is open. To start without
  opening the app, use the OS shortcuts (Siri, Action button, assistant
  gesture). Android hands-free mode is an explicit opt-in.

## Effort

### Desktop

| Work | Estimate |
|---|---|
| Spike in `spikes/`: keyword spotting on Windows and macOS with sherpa-onnx and tract, measuring detection rate and false triggers per hour for 3–4 candidate names | 2–3 days |
| Core: detector trait in `uia-core`, hook at the capture check, pre-wake buffer, follow-up window, tests with a fake detector | 3–4 days |
| `uia-wake` crate: features, resampling to 16 kHz, model loading, thresholds | 3–4 days (+1–2 weeks for the pure-Rust decoder) |
| Config `[activation]` section, Settings UI, HUD state | 2–3 days |
| Packaging, `NOTICE.txt` and licences, manual test checklist, tuning on real hardware | 2–3 days |

### Mobile (on top of the planned SP6 mobile stage)

| Work | Estimate |
|---|---|
| Wake word while the app is open, reusing `uia-wake` | 2–3 days if pure Rust; about a week with C++ libraries |
| Android hands-free foreground service plugin | about 1 week |
| iOS App Intents, Siri shortcut and Control Center control | 3–5 days |

## Spike go/no-go

On both Windows and macOS:

- at least about 90% of wake phrases detected;
- no more than about one false trigger per 8 hours of normal office
  background audio.

If the spike misses this, fall back to **hold-to-talk on the existing
global shortcut** (about 1–2 days). It is not hands-free, but it removes
the need to click the mute button.

## To verify during the spike

These come from general knowledge, not from checks made while writing
this proposal:

- tract and candle support for the chosen model's operations;
- the licence of the specific sherpa-onnx keyword-spotting model;
- current iOS and Android background-audio and foreground-service rules;
- installer size added by the model files.
