# Provider A/B — OpenAI Realtime vs Nova Sonic 2

Measured 2026-08-20 (S14). Every number below came from
`crates/uia-app/tests/provider_ab.rs`, run from Sydney against the live
services.

```
OPENAI_API_KEY=...  cargo test -p uia-app --test provider_ab \
    -- --ignored --nocapture --test-threads=1
AWS_PROFILE=sso-nonprod  cargo test -p uia-app --test provider_ab \
    nova_provider_ab -- --ignored --nocapture --test-threads=1
```

## What was measured, and what it is not

Ten exchanges per engine. Each is a **fresh session**: a shared session would
let turn N's context change turn N+1's latency, and the two providers cache
differently.

The prompt is **synthesised speech**, not a microphone — OpenAI TTS
(`gpt-4o-mini-tts-2025-12-15`, pinned), streamed at real-time 20 ms pacing, the
same technique S16 used to prove audio moves. This is deliberate: through a
human at a microphone the wording, the pace of speech, the room, and the moment
of hotkey release move the result more than the engines differ from each other,
so nothing would be reproducible or comparable. Every exchange here is
byte-identical across engines and across reruns.

**Time-to-first-audio** is measured from the last sample of *speech* sent to the
first decoded assistant chunk with real signal in it (RMS > 0.002). It therefore
includes each provider's own end-of-turn silence detection, which is genuinely
part of what a user waits through. It is **not** the "hotkey release to first
speaker sample" figure the task plan describes: it excludes capture, the device
buffer, and playback, all of which are identical between the two engines anyway.

A methodology note worth keeping, because the first run got it wrong: TTS pads
the end of every clip with silence, and both providers' VADs start their
end-of-turn timer *during that padding*. The first run therefore recorded a
5 ms time-to-first-audio — not a fast model, a clock started after the answer
had already begun. Fixtures are now trimmed of trailing silence
(`trim_trailing_silence`), which collapsed the spread from 5–2104 ms to
1201–1868 ms for the same engine.

**Tool-calling reliability** is ten differently-worded prompts that should each
call `clock.now` with a city. Scored on whether the call happened *and* the
`city` argument matched, case-insensitively.

## Results

| | `gpt-realtime-2.1-mini` | `gpt-realtime-2.1` | `nova-2-sonic` (ap-northeast-1) |
|---|---:|---:|---:|
| Exchanges | 10 | 10 | 10 |
| Time-to-first-audio, median | **1588 ms** | **1582 ms** | **1970 ms** |
| Time-to-first-audio, mean | 1508 ms | 1523 ms | 1961 ms |
| Range | 1201–1868 ms | 1317–1737 ms | 1725–2266 ms |
| Tool called | 10/10 | 10/10 | 10/10 |
| Tool called with the right city | **10/10** | **10/10** | **10/10** |
| Errors | none | none | none |

Raw samples (ms, sorted):

```
mini  : [1201, 1242, 1262, 1272, 1374, 1588, 1616, 1802, 1860, 1868]
full  : [1317, 1328, 1377, 1392, 1513, 1582, 1612, 1648, 1728, 1737]
nova  : [1725, 1774, 1846, 1863, 1943, 1970, 1999, 2044, 2184, 2266]
```

## Region and RTT — how much of the gap is geography

TCP handshake RTT from the same machine, same session, 10 samples each:

| Endpoint | Median RTT |
|---|---:|
| `api.openai.com` | **24.5 ms** |
| `bedrock-runtime.ap-northeast-1.amazonaws.com` (Tokyo) | **227.3 ms** |
| `bedrock-runtime.us-east-1.amazonaws.com` (for reference) | 239.1 ms |

**This is the most important row in the document.** The observed
time-to-first-audio gap between OpenAI mini and Nova is ~382 ms at the median.
The pure network RTT difference is ~203 ms — and a bidirectional stream spends
more than one round trip getting from "last audio in" to "first audio out", so
**most of the measured gap is geography and transport, not the model.**

Nova Sonic 2 has no `ap-southeast-2` presence and no global endpoint, so every
Sydney session is a Sydney↔Tokyo round trip over TCP. OpenAI's leg is WebRTC
over UDP to an edge 24 ms away. Attributing the whole 382 ms to model quality
would be wrong, and would point at the wrong fix.

`us-east-1` is no help: it measures *worse* than Tokyo from here. There is no
region that makes Nova local to Sydney.

## The three questions this stage exists to answer

### 1. Is the fallback actually viable?

**Yes — usable but worse.** Nova answers about 380 ms slower at the median, with
no errors and identical tool-calling accuracy. ~2 s to first audio is slower
than pleasant but well inside usable for a fallback that only runs when the
primary cannot. The engine-priority decision does not need revisiting on
viability grounds, and `PipelineEngine` is **not** warranted on this evidence.

### 2. Does the primary decision survive contact?

**Yes.** Nova did not measure better; it measured consistently worse, and its
*best* sample (1725 ms) is slower than the primary's median. OpenAI Realtime
stays the default.

### 3. Is the Nova leg's transport the bottleneck, or the model?

**Transport, predominantly** — see the RTT table above. This is the specific
trigger PLAN.md names for revisiting uniform WebRTC via AgentCore Runtime at
SP2/SP4. The finding supports keeping that option open, and equally supports
*not* exercising it now: the A$60.92/month floor (driven almost entirely by the
NAT Gateway VPC mode forces) buys back at most ~200 ms on an engine that is only
reached when the primary is down.

## mini vs the full model

**No measurable difference, so mini stays the default.** 1588 ms vs 1582 ms at
the median is noise at n=10; both scored 10/10 on tool calling; both answered
every prompt correctly and briefly. Mini is ~3.2× cheaper on audio and ~10× on
text, and this stage found nothing to pay that for. The full model's range is
slightly tighter (1317–1737 vs 1201–1868), which is not worth 3.2×.

## Defects this A/B found

Every one of these was invisible before S14, for one reason: **the contract
suite declares no tools**, so no stage had ever completed a tool round trip
against either real service.

1. **Nova enforces `^[a-zA-Z0-9_-]+$` on tool names, exactly as OpenAI does.**
   PLAN.md's frozen decision said "Nova accepts dots, so the two engines
   legitimately declare different name strings" — that was never tested and is
   wrong. Live:

   ```
   ValidationException: Malformed input request:
   #/toolConfig/tools/0/toolSpec/name: string [clock.now] does not match
   pattern ^[a-zA-Z0-9_-]+$
   ```

   Nova's failure mode is **worse** than OpenAI's: OpenAI drops the tool
   declaration and the session survives; Nova returns a 400 and there is no
   session at all. One configured MCP tool made the entire fallback engine
   unusable. Fixed by moving the `.` → `__` translation and its inverse into
   `uia-mcp` (which both engines already depend on) and applying it in Nova.

2. **A Nova tool result must be a stringified JSON document**, not the bare
   sentence S10 sent: `ValidationException: Tool Response parsing error`.

3. **...but the media type must still be `text/plain`.** `application/json` is
   rejected by name — `Tool result media type must be one of: [text/plain]` —
   even though the content it carries has to be JSON. Neither half of this is
   guessable from the shape of the request; each was established by making the
   mistake against the service.

4. **The AWS SDK's error `Display` hides everything that matters.** All three of
   the above first appeared as the string `service error` and nothing else. The
   `ValidationException` and its message live one or more levels down the
   `source()` chain. `classify_sdk_error` now walks it — the first Nova run
   produced ten identical, useless lines and cost a full run to diagnose.

## Cost

Negligible at this scale: 30 exchanges of a few seconds of audio each. Recorded
because the run is repeatable and someone will want to know before rerunning it
— roughly a few US cents for the whole document, dominated by the full model's
pass at ~3.2× mini's audio rate.
