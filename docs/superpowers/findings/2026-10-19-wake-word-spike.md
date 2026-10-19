# Wake-word spike: sherpa-onnx vs tract, four candidate names

Measured 2026-10-18 to 2026-10-19 on Windows 11 (x86_64, MSVC). Spec:
[2026-10-10-wake-word-activation-design.md](../specs/2026-10-10-wake-word-activation-design.md).
Plan: `docs/superpowers/plans/2026-10-18-wake-word-spike.md` (local only, not
published). Model: `sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01`
(English, BPE, about 3.3M parameters).

Every number below came from `spikes/wake-probe`. That probe is **local only**:
`spikes/` is gitignored, and the probe has its own nested git repo, so none of
its code, audio or reports are in this repository. Run from `spikes/wake-probe`
in Git Bash, with `P=target/release/wake-probe.exe`:

```
bash tools/get-model.sh                                   # model into models/
cargo build --release --locked                            # sherpa-onnx 1.13.1, sys pinned =1.13.1
cargo run --release --locked -- live --label HEY_JUNIPER  # live RTF line every minute

# corpus (Task 6S): TTS positives, noisy mixes, LibriSpeech negatives
$P tts                                                    # needs OPENAI_API_KEY or gpt-api.key
.venv/Scripts/python tools/build_corpus.py vet --delete-broken && $P tts
.venv/Scripts/python tools/build_corpus.py positives      # tts-snr10/, tts-snr0/ (seed 1)
.venv/Scripts/python tools/build_corpus.py negatives      # libri-noisy/ (seed 0)
$P record --out data/negatives/win-room --hours 1         # real room, production mic

# evaluation (Task 8)
$P eval-positives --alias HEY_MY_MY=HEY_MYMY --alias HEY_MARIGOLD_PH=HEY_MARIGOLD
$P eval-negatives
.venv/Scripts/python tools/strike.py data/reports/eval-negatives-<ts>.json

# tract parity (Task 7); for the patched numbers uncomment [patch.crates-io] in Cargo.toml
M=models/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01; S=epoch-12-avg-2-chunk-16-left-64
for k in encoder decoder joiner; do
  .venv/Scripts/python tools/reference.py --model $M/$k-$S.onnx --kind $k --out data/refs/$k
  .venv/Scripts/python tools/reference.py --model $M/$k-$S.int8.onnx --kind $k --out data/refs/$k-int8
done
cargo run --release --features tract -- tract-parity [--int8]
```

The reports that the tables were built from are
`data/reports/eval-positives-1792366817.json`,
`data/reports/eval-negatives-1792374402.json` and its `.struck.json`. A
confirmation rerun at each chosen threshold gave identical counts.

## What was measured, and what it is not

**The corpus is entirely synthetic, apart from one hour of real room audio.**
The spike plan called for 8+ hours of office audio and human recordings of
each name. Neither was possible: the user lives alone, works from home, and
asked Claude to build the corpus. Audio played through this PC into its own
echo-cancelled mic is removed by the AEC by design, so the corpus was built
offline instead (plan, Task 6S addendum):

| Source | What it is | Size |
|---|---|---|
| **TTS (clean)** | OpenAI TTS: 11 voices × 4 styles (natural, fast, quiet, loud) × 2 phrasings ("Hey X" / "Hey X, what's on today?") | 88 clips per name |
| **tts-snr10** | each TTS clip + LibriSpeech babble + pink noise at 10 dB SNR (active-speech SNR) | 88 per name |
| **tts-snr0** | the same mixes at 0 dB SNR | 88 per name |
| **libri-noisy** (negatives) | LibriSpeech dev/test read speech (CC BY 4.0) with 0.5–3 s gaps, pink noise and 50 Hz hum | 26.68 h |
| **win-room** (negatives) | the user's room through the production mic (WASAPI Communications, AEC on) | 1.00 h |

How to read each condition:

- **Clean TTS is the proxy for the user's environment**, which is a quiet home
  with no other talkers. It is also an upper bound for human speech, since TTS
  is clean and well-articulated.
- **snr10** stands for a TV or other people talking in the room.
- **snr0** stands for loud babble.

No single condition is "the realistic one". All three are shown.

**libri-noisy is a worst case for false triggers.** It is dense, continuous
speech with almost no silence, much harsher than a quiet home or a normal
office. Its transcripts were used to strike true mentions automatically. Only
one exists, the bare word "JUNIPER", and it never fired. That is expected:
the keyword is "HEY JUNIPER", so this is not a recall failure. The strike
removed 0 events.

**win-room was not silent.** It has RMS 1623 and a clipped peak of 32767,
and 3% of its 1 s windows were near-silent. About 3 s were lost at a device
reopen.

**What this cannot show:**

- human speakers;
- far-field acoustics and reverberation;
- macOS, which the gate requires;
- the assistant waking itself through its own speakers.

**Nothing was listened to.** Thresholds were chosen on the same data they
are reported on, which is optimistic.

## 1. Decision

| Scope | Decision |
|---|---|
| **Desktop, quiet room, opt-in** | **Provisional GO**, with **sherpa-onnx 1.13.1** behind the `Detector` trait in `uia-wake`. |
| **Noisy room** (TV, other talkers) | **NO-GO with this 3.3M model.** The best name detects 33/88 (37.5%) at 10 dB SNR. |
| **Engine: tract** | **Not today.** Stock tract 0.23.4 cannot load the encoder. With a one-line upstream fix it matches onnxruntime and is fast enough, so it can ship later behind the same trait. |
| **Short names (MAX)** | **Reject** in Settings guidance: 3.00 false triggers per 8 h at best (95% upper bound 5.09). |

Why this decision:

- **Two names cleared both bars in the quiet-home proxy.** HEY_JUNIPER
  detected 84/88 (95.5%) and HEY_MY_MY detected 81/88 (92.0%) on clean TTS.
  Both had 0 false triggers in 26.68 h of worst-case dense speech (95% upper
  bound 0.89 per 8 h).
- **Why "provisional":**
  - every positive is synthetic;
  - there is no far-field data;
  - macOS was not measured, and the spec requires both platforms;
  - the threshold was chosen on the data it is reported on.

  HEY_MY_MY's interval reaches down to 84.5% (Wilson 95%: 84.5–96.1%), so the
  margin over "about 90%" is thin.
- **Why not a plain GO:** the gate is written for "normal office background
  audio" with human speakers. The run had neither, and it showed that
  detection collapses once other talkers are present.
- **Why not a NO-GO:** the measured environment does pass, and the feature is
  opt-in. Falling back to hold-to-talk would throw away a working quiet-room
  result.
- **For noisy rooms**, the plan's Global Constraints name the next step: the
  trained-classifier follow-up spike (livekit-wakeword, Apache-2.0). It would
  train per-name classifiers offline and score them on this corpus.
  Hold-to-talk is the fallback only if that also fails.

**To make the decision final:**

- a human `prompt-record` session, scored (README Task 6, Step 2);
- the same run on macOS.

If human detection on Windows falls below about 90% for both names, the
quiet-room GO becomes a NO-GO and the livekit-wakeword follow-up applies.

## 2. Gate table

Bars: detection ≥ about 90%, and false triggers (FT) ≤ about 1 per 8 h. The FT
columns are the same for every condition of a name, because the negatives
don't depend on the positives' noise. "upper" is the Poisson 95% upper bound.
FT is from libri-noisy (26.68 h, the worst case) and win-room (1.00 h, the
real room).

| Platform | Name | Condition | t | Detection n/N | Wilson 95% | FT/8 h libri (upper) | FT/8 h room (upper) | Gate |
|---|---|---|---|---|---|---|---|---|
| Windows | HEY_JUNIPER | clean TTS (quiet home) | 0.30 | **84/88 = 95.5%** | 88.9–98.2% | 0.00 (0.89) | 0.00 (23.77) | **pass** (synthetic) |
| Windows | HEY_JUNIPER | tts-snr10 (TV/talkers) | 0.30 | 33/88 = 37.5% | 28.1–47.9% | 0.00 (0.89) | 0.00 (23.77) | fail (detection) |
| Windows | HEY_JUNIPER | tts-snr0 (loud babble) | 0.30 | 2/88 = 2.3% | | 0.00 (0.89) | 0.00 (23.77) | fail (detection) |
| Windows | HEY_MY_MY (`▁MY ▁MY`) | clean TTS | 0.20 | **81/88 = 92.0%** | 84.5–96.1% | 0.00 (0.89) | 0.00 (23.77) | **pass** (synthetic, thin) |
| Windows | HEY_MY_MY | tts-snr10 | 0.20 | 22/88 = 25.0% | 17.1–35.0% | 0.00 (0.89) | 0.00 (23.77) | fail (detection) |
| Windows | HEY_MY_MY | tts-snr0 | 0.20 | 5/88 = 5.7% | | 0.00 (0.89) | 0.00 (23.77) | fail (detection) |
| Windows | HEY_MYMY (`▁MY M Y`) | all three | any | 0/88 at every t | | 0.00 (0.89) | 0.00 (23.77) | fail (detection) |
| Windows | MAX | clean TTS | 0.40 (lowest FT) | 72/88 = 81.8% | | **3.00 (5.09)** | 0.00 (23.77) | **fail (FT)**, at every t |
| Windows | MAX | tts-snr10 / snr0 | 0.40 | 38/88 = 43.2% / 8/88 = 9.1% | | 3.00 (5.09) | 0.00 (23.77) | fail |
| Windows | HEY_MARIGOLD (`▁MAR IG OL D`) | clean / snr10 / snr0 | 0.30 | 2/88 / 1/88 / 0/88 | | 0.00 (0.89) | 0.00 (23.77) | fail (detection) |
| Windows | HEY_MARIGOLD_PH ("MERRY GOLD") | clean / snr10 / snr0 | 0.40 | 38/88 = 43.2% / 15/88 / 3/88 | | 0.00 (0.89) | 0.00 (23.77) | fail (detection) |
| Windows | any | human speakers | | **not measured** | | | | not measured |
| Windows | any | far field (2 m) | | **not measured** | | | | not measured |
| macOS | any | any | | **not measured** | | **not measured** | **not measured** | not measured |

Notes:

- **Threshold choice.** t is Task 8's operating point: the highest snr10
  detection whose libri FT ≤ 1 per 8 h, with ties going to the higher
  threshold. The sweep was 0.15, 0.20, 0.25, 0.30 and 0.40, with score 1.
  Ranking on clean TTS instead would pick lower thresholds, at the same 0 FT:
  HEY_JUNIPER 85/88 at t = 0.20 and HEY_MY_MY 84/88 at t = 0.15. The table
  keeps Task 8's points, so its clean figures are slightly conservative
  within the sweep.
- **HEY_MY_MY is threshold-sensitive:** 84/88 at 0.15, 81/88 at 0.20, 73/88
  at 0.25 and 60/88 at 0.30. HEY_JUNIPER is flat between 0.15 and 0.30
  (85/88 to 84/88).
- **MAX at its most sensitive** (t = 0.15) detects 86/88 clean but fires 17
  times in 26.68 h (5.10 per 8 h, upper 7.65).
- **No cross-firing on clean TTS:** in the Task 6S-a run, no clip fired a
  different name's keyword. Task 8 did not tabulate cross-fires for the
  noisy mixes.
- **The room bound of 23.77 per 8 h is too wide to mean anything.** 0
  triggers in 1 h is weak evidence; libri-noisy carries the FT result.

## 3. Engine

| | sherpa-onnx 1.13.1 | tract 0.23.4 (stock) | tract 0.23.4 + Range fix (vendored, disabled) |
|---|---|---|---|
| Build on Windows/MSVC | **OK** from the downloaded prebuilt static lib (`win-x64-static-MT-Release`), no C++ toolchain. It needs `sherpa-onnx-sys = "=1.13.1"`: a caret resolve picks 1.13.8, which fails with E0063 (missing struct fields). Benign LNK4098 LIBCMT warning. | OK (`cargo build --features tract`) | OK |
| Load encoder | OK | **FAILS** at node `/Range`: "Impossible to unify Val(16) with Val(18)" (fp32 #321, int8 #346) | OK |
| Parity vs onnxruntime, fp32 (bar 1e-3) | (reference engine) | decoder 9.54e-7, joiner 1.14e-5: PASS; encoder: n/a | **all 39 encoder outputs PASS over 20 carried steps, worst 3.24e-5**; decoder and joiner PASS |
| Parity, int8 | — | decoder and joiner PASS; encoder: n/a | **encoder FAILS**: `encoder_out` 5.21e-2, states 6.17e-3 to 4.43e-1 |
| RTF, 1 thread (bar 0.1) | **0.0198** live over 60 s, mostly quiet input, everything included | n/a | fp32 **0.0251** (1.27× sherpa); int8 0.1015 (FAIL) |

- **The tract bug.** It is in tract-hir 0.23.4, `src/ops/array/range.rs`.
  The shape rule computes `(limit - start).divceil(delta)` with a negative
  delta cast to `usize`, which gives 18 instead of 16 for any negative step.
  This was confirmed in source. tract-core's typed Range is correct. The fix
  is one line (signed ceil-division) and matches the typed op. No other op
  failed after it.
- **The tract RTF is an upper-bound estimate:** encoder 6.44 ms per 320 ms
  chunk, plus 8 frames × 4 paths × (joiner + decoder). sherpa-onnx's figure
  is measured live and includes feature extraction. The two are the same
  order of magnitude, not an exact comparison.
- **What a pure-Rust ship needs:** the Range fix released upstream, and our
  own transducer keyword search and fbank features (the spec's +1–2 weeks).
  int8 in tract is both slower and inaccurate, so ship fp32.
- **Binary size:** in the probe, tract-onnx added 24.4 MB, more than the
  whole 22.3 MB sherpa-onnx build (section 5). The case for tract is build
  and mobile cross-compilation, not size.

## 4. Names and tokenisation

**Spelling-based tokens fail. This is the most product-relevant finding.**
The model only matches the BPE pieces it is given, and the BPE of a written
invented name is not how it is said:

| Keyword | Tokens | Clean TTS detection |
|---|---|---|
| HEY_MYMY (written "MyMy") | `▁HE Y ▁MY M Y` | **0/88 at every threshold, every source** |
| HEY_MY_MY (spoken "my my") | `▁HE Y ▁MY ▁MY` | 81/88 at t = 0.20 |
| HEY_MARIGOLD (written) | `▁HE Y ▁MAR IG OL D` | 3/88 = 3.4% (t = 0.25) |
| "HEY MARRY GOLD" | | 13.6% (t = 0.25) |
| "HEY MARI GOLD" | | 31.8% (t = 0.25) |
| "HEY MERRY GOLD" (HEY_MARIGOLD_PH) | `▁HE Y ▁ME R RY ▁GO L D` | 55.7% (t = 0.25); 38/88 = 43.2% at t = 0.40 |

The Marigold spellings come from a controller experiment on the same 88
clips at t = 0.25. Only MERRY GOLD was carried into the full Task 8 run.

**Consequence for `uia-wake`: the name-to-token step must not be a naive BPE
of the written name.** Options, cheapest first:

1. **Try several spellings.** Write the name as separate common words
   ("My My", "Merry Gold") and register several spellings as alternative
   keywords for the same label.
2. **Validate a candidate name.** When a name is set, synthesise it (TTS, or
   a short user recording) and accept it only if the detector fires on its
   own rendering. Warn in Settings if it doesn't.
3. **Phonetic mapping:** grapheme-to-phoneme, then pick the BPE spelling
   closest to the phonemes. This is the most work.

**Is "MyMy" a safe default for `wake_word = ""`?**

- Only if the persona's spoken name goes through the step above, so that
  "MyMy" becomes `▁MY ▁MY`. Passed as written, the default never wakes.
- Even then it is the weaker of the two passing names: 92.0% with a lower
  bound of 84.5%, and threshold-sensitive.
- Every passing phrase had the "Hey" prefix. A bare persona name was not
  measured; the only bare name, MAX, failed on false triggers.
- **Recommendation:** default to "Hey" + the persona name, validated through
  the TTS check (option 2).

**What false-triggered.** Only MAX: 10 to 17 times in 26.68 h, depending on
threshold. No transcript in those hours contains "MAX", so these are
genuinely false. Their causes are unknown: the excerpts in
`data/reports/false-triggers/libri-noisy/MAX-t*` were not listened to.

**Suggested Settings guidance text:**

> Pick a wake phrase of 3–4 syllables that starts with "Hey", such as "Hey
> Juniper". Short names like "Max" wake by mistake several times a day.
> Unusual names may need to be spelled the way they sound: "Merry Gold"
> rather than "Marigold", "My My" rather than "MyMy". After you change the
> name, say it once to check that it wakes. Wake words work best in a quiet
> room; with a TV or other people talking, use the shortcut or the mute
> button. Don't use "Siri" or "Alexa": they would wake real devices nearby.

## 5. The spec's "to verify" list

| Item | Result |
|---|---|
| tract and candle op support | **tract 0.23.4:** every op loads except one shape-inference bug in tract-hir's `Range` with negative steps. With the one-line fix, fp32 parity passes on all graphs. **candle: not tested.** |
| Licence of the sherpa-onnx KWS model | **Open.** The archive ships no licence file. The README front matter (ModelScope metadata) says `license: Apache License 2.0`. It was trained on GigaSpeech XL, which has its own terms. Confirm with k2-fsa, or find a licence file upstream, before shipping, and record it in `NOTICE.txt`. |
| Installer size added by the model | **Model files:** encoder 12M fp32 / 4.6M int8, decoder 1.1M / 272K, joiner 628K / 160K (`du -sh`). That is about 13.8 MB for fp32 and about 5.0 MB for int8, plus `tokens.txt` and `bpe.model` (239K). The whole model dir is 20M including test WAVs. **Static sherpa-onnx lib:** the downloaded `sherpa-onnx-v1.13.1-win-x64-static-MT-Release-lib.tar.bz2` is 114.2 MB (114,222,392 B), but that is the full static library set, and the linker keeps only what is used. The tighter bound is the whole probe: `wake-probe.exe` release, **without tract, is 22.3 MB** (22,294,016 B). That includes sherpa-onnx, onnxruntime, tokio, reqwest and uia-audio. **With `--features tract` it is 46.6 MB** (46,644,736 B), so tract-onnx adds **24.4 MB** of Rust code on top. "Pure Rust" is therefore not smaller. Both were built 2026-10-19 with `cargo build --release --locked [--features tract]`. Shipping fp32 adds about 13.8 MB of model files. Note that sherpa-onnx int8 accuracy was not measured. |
| iOS background audio rules | **Unchanged from spec.** App Review Guideline 2.5.4: background services only "for their intended purposes: VoIP, audio playback, location, task completion, local notifications, etc." Guideline 2.5.14 requires "a clear visual and/or audible indication when recording". Always-on background listening for a wake word fits neither well. Source: <https://developer.apple.com/app-store/review/guidelines/>, accessed 2026-10-19. Apple's background-modes page (<https://developer.apple.com/documentation/xcode/configuring-background-execution-modes>) did not render for the fetcher, so the `audio` mode details were **not re-verified**. |
| Android foreground service `microphone` | **Confirmed, unchanged from spec.** Apps targeting Android 12+ cannot start a foreground service from the background (`ForegroundServiceStartNotAllowedException`) except in listed cases. `microphone` needs `FOREGROUND_SERVICE_MICROPHONE` plus runtime `RECORD_AUDIO`. Since Android 14, `RECORD_AUDIO` is while-in-use, so the service "cannot be created while your app is in the background", even with most exemptions. It must be started while an activity is visible, or from a notification or widget interaction. Sources: <https://developer.android.com/develop/background-work/services/fgs/restrictions-bg-start> and <https://developer.android.com/about/versions/14/changes/fgs-types-required>, both accessed 2026-10-19. |

## 6. Caveats

- **Speakers:**
  - there are zero human speakers;
  - the 11 TTS voices are studio-clean, so they are an upper bound on human
    detection;
  - "quiet" and "loud" styles were the only stand-ins for distance.
- **Room:**
  - libri-noisy is dense read speech plus synthetic noise, not a real room;
  - the one real hour (win-room) is too short to bound FT usefully (upper
    23.77 per 8 h);
  - no far field or reverberation was tested.
- **Optimistic threshold:** the threshold was picked on the same data it is
  reported on. With n = 88, the 95% interval is about ±5 points near 92–95%.
- **Not all TTS clips are short:** 21 of the 352 clips have more than 5 s of
  speech. They were kept, since extra speech cannot hide a detection, but
  they may slightly inflate rates if the TTS repeated the name.
- **Self-wake through the speakers was not measured.** The AEC reference is
  the endpoint mix, so it should cancel the assistant saying its own name,
  but nothing was tested.
- **Only Windows was compiled.** The probe's macOS capture branch has never
  been compiled.
- **The sherpa RTF of 0.0198** was measured on mostly quiet input. Keyword
  search cost grows with active paths, so busy audio may cost more; it is
  still far below the bar.
- **Babble robustness is the model's, not the corpus's.** An snr10 clip
  was measured at 10.5 dB active-speech SNR, so the mixes are correct.

## 7. Next steps

1. **A human `prompt-record` session on Windows** (README Task 6, Step 2).
   Record 15 clips at the desk and 15 from about 2 m for HEY_JUNIPER and
   HEY_MY_MY, with a second speaker if possible. Score them at t = 0.30 and
   t = 0.20. This turns "provisional" into go or no-go for Windows.
2. **macOS:** copy the probe to a Mac, compile the capture branch, record 8+
   hours of room audio plus human positives, and fill in the macOS rows.
3. **The `uia-wake` implementation plan**, a new plan:
   - the `Detector` trait in `uia-core`;
   - the hook at `pump_audio`'s `capture_enabled()` check;
   - the pre-wake ring buffer and the follow-up window;
   - sherpa-onnx with `sherpa-onnx-sys = "=1.13.1"` pinned in the manifest;
   - the name-to-token step (several spellings, plus a TTS self-check);
   - the Settings guidance above.
4. **The livekit-wakeword follow-up spike** for noisy rooms. It trains
   per-name classifiers offline for the default name and a shortlist, and
   scores them on `tts-snr10` / `tts-snr0` and libri-noisy. The corpus is
   kept for it.
5. **Report the tract-hir Range bug upstream** (sonos/tract), with the
   failing node, the formula and the one-line fix. Once it is released,
   re-run `tract-parity` without the vendored patch.
6. **Confirm the model licence** with k2-fsa / the ModelScope page, plus
   GigaSpeech's terms, before the model goes into an installer and
   `NOTICE.txt`.
