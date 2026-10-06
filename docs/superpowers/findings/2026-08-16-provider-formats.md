# Provider formats and wire shapes — Nova Sonic and OpenAI Realtime

**Produced by:** S1 provider spikes (`spikes/nova-probe`, `spikes/openai-probe`).
**Verified live:** 2026-08-17, against both real services. Every number below came
back from the provider; nothing here is copied from a doc without a live check.

The probes are throwaway. This document is the deliverable — S9 (OpenAI engine),
S10 (Nova engine), S11 (contract suite), and S12 (config) consume it.

---

## 1. Side by side

| | **Nova Sonic** | **OpenAI Realtime** |
|---|---|---|
| Model id | `amazon.nova-2-sonic-v1:0` | `gpt-realtime-2.1` |
| Region / endpoint | `ap-northeast-1` (no global endpoint) | `api.openai.com` |
| Transport | HTTP/2 bidirectional stream, SigV4 | WebRTC (DTLS/SRTP + SCTP data channel) |
| Auth | AWS credentials, SigV4 per request | ephemeral client secret, minted with the API key |
| Input audio | LPCM **16-bit mono**, `8000 \| 16000 \| 24000` Hz, base64 in JSON | Opus 48 kHz on the wire; **24 kHz PCM** at our boundary |
| Output audio | LPCM 16-bit mono at the rate **we request** (8/16/24 kHz) | Opus 48 kHz stereo on the wire; 24 kHz PCM at the API boundary |
| Framing | ~32 ms `audioInput` events, base64 JSON | RTP packets on a media track; no framing of ours |
| Barge-in signal | `userSpeechStart` / `userSpeechEnd` events | `server_vad` with `interrupt_response: true` |
| Tool declaration | `promptStart.toolConfiguration.tools[]`, schema as a **JSON string** | `session.update.session.tools[]`, schema as a **JSON object** |

The two rows that matter most for `uia-audio`: **the resampler targets 24 kHz for
OpenAI and 16 kHz for Nova** (16 kHz is the sweet spot for Nova's ASR and the only
rate all three of Polly, cpal defaults, and Nova agree on cheaply), and **only Nova
ever sees raw PCM from us**. For OpenAI, Opus encoding happens inside the WebRTC
stack; the 24 kHz PCM figure is what `AudioFormat` must report so our own capture
path resamples correctly before handing samples to the encoder.

---

## 2. Nova Sonic

### 2.1 Opening the stream

`InvokeModelWithBidirectionalStream`, `aws-sdk-bedrockruntime` 1.139.0. The
operation's `body` is a required
`EventStreamSender<InvokeModelWithBidirectionalStreamInput, …Error>`, built from any
`Stream<Item = Result<T, E>> + Send + Sync + 'static`. `tokio_stream::wrappers::ReceiverStream`
over an `mpsc::channel` works.

> **Gotcha, paid for once.** The session-setup events must already be queued in the
> channel *before* `send()` is awaited. Nova emits no response headers until it has
> received `sessionStart`, and `send()` does not return until those headers arrive.
> Queue afterwards and the program hangs forever with no error and no output — the
> first probe run did exactly that.

Each stream item is a `BidirectionalInputPayloadPart` whose `bytes` are the UTF-8
JSON of one event. Responses arrive as
`InvokeModelWithBidirectionalStreamOutput::Chunk(part)`; decode `part.bytes()` as
UTF-8 JSON. Note the SDK's `Debug` impl redacts the payload as
`*** Sensitive Data Redacted ***`, so logging the enum is useless — decode the bytes.

### 2.2 Audio formats — measured, not assumed

Input configuration is declared per content block in `contentStart.audioInputConfiguration`.
Declaring 44100 Hz is rejected, and the error enumerates the real set:

```
STREAM ERROR: ValidationException: Invalid audio format:
audio/lpcm;sample-rate=44100;sample-size-bits=16;channel-count=1;encoding=base64.
Expected LPCM format with sample rate in [8000, 16000, 24000],
sample size bits of 16 and channel count of 1
```

Both 16000 and 8000 were accepted and transcribed correctly in full runs.

- **Input:** `audio/lpcm`, `sampleRateHertz` ∈ {8000, 16000, 24000}, `sampleSizeBits` 16,
  `channelCount` 1, `encoding` `base64`, `audioType` `SPEECH`. **No other combination is valid.**
- **Output:** whatever we asked for in `promptStart.audioOutputConfiguration` — the
  service echoes it back on the audio `contentStart`. We requested 24000/16/1 and got
  exactly that. `voiceId` `matthew` was accepted.
- **Output chunking:** 5120 base64 characters per `audioOutput` event = 3840 bytes =
  1920 samples = **80 ms per chunk** at 24 kHz. A ~2 s reply arrived as 33–35 chunks.

### 2.3 Event vocabulary (observed verbatim)

Input (client → model), in order: `sessionStart` → `promptStart` →
`contentStart`/`textInput`/`contentEnd` (SYSTEM) → `contentStart`(AUDIO) →
`audioInput`×N → `contentEnd` → `promptEnd` → `sessionEnd`. Tool results go back as
`contentStart`(TOOL) → `toolResult` → `contentEnd`.

Output (model → client):

| Purpose | Event | Notes |
|---|---|---|
| user started speaking | `userSpeechStart` | carries `inputAudioOffsetMs`. **Not in the public docs** — this is the barge-in trigger |
| user stopped speaking | `userSpeechEnd` | `inputAudioOffsetMs` + `inputAudioDetectionOffsetMs` |
| turn opens | `completionStart` | `sessionId`, `promptName`, `completionId` |
| ASR transcript | `contentStart` type `TEXT`, role `USER`, `additionalModelFields:{"generationStage":"FINAL"}` → `textOutput` → `contentEnd` | lower-cased: `"what time is it in sydney?"` |
| tool call | `contentStart` type `TOOL` → `toolUse` → `contentEnd` (`stopReason: TOOL_USE`) | `toolUse` has `toolName`, `toolUseId`, `content` (stringified JSON args) |
| planned speech | `contentStart` role `ASSISTANT`, `generationStage: SPECULATIVE` → `textOutput` → `contentEnd` | text preview of the reply |
| spoken audio | `contentStart` type `AUDIO` → `audioOutput`×N → `contentEnd` (`stopReason: END_TURN`) | base64 LPCM |
| token usage | `usageEvent` | delta + running totals, speech and text separated |
| completion | `completionEnd` (`stopReason: END_TURN`) | see below |

> **`completionEnd` does not arrive on its own.** With the input audio content block
> still open, a 90 s wait produced no `completionEnd` even though the assistant's audio
> block had already closed with `stopReason: END_TURN`. Sending `contentEnd` for the
> audio input and then `promptEnd` releases it immediately. **The turn boundary the
> session state machine should act on is the audio `contentEnd` with
> `stopReason: END_TURN`, not `completionEnd`** — waiting for the latter deadlocks a
> continuous conversation.

### 2.4 Tool declarations

Declared once, at `promptStart`:

```json
"toolConfiguration": {"tools": [{"toolSpec": {
  "name": "get_time",
  "description": "Get the current local time for a city.",
  "inputSchema": {"json": "{\"type\":\"object\",\"properties\":{\"city\":{\"type\":\"string\"}},\"required\":[\"city\"]}"}
}}]}
```

`inputSchema.json` is a **stringified** JSON schema, not a nested object. The
translator in S8 must `serde_json::to_string` the schema for Nova and pass it as an
object for OpenAI — that asymmetry is the whole reason the translation layer exists.

Round trip observed: `toolUse{toolName:"get_time", content:"{\"city\":\"sydney\"}"}`
→ we replied with `toolResult{content:"{\"time\":\"00:54\",\"city\":\"Sydney\"}"}` →
the model spoke *"The current time in Sydney is 00:54."*

---

## 3. OpenAI Realtime

### 3.1 Credentials and the SDP exchange

1. `POST https://api.openai.com/v1/realtime/client_secrets`, bearer = the long-lived
   key, body `{"session":{"type":"realtime","model":"gpt-realtime-2.1"}}`.
   Response: `value` (a **35-character** ephemeral secret) at the top level, plus
   `expires_at` and a full `session` object.
2. `POST https://api.openai.com/v1/realtime/calls?model=gpt-realtime-2.1`, bearer =
   the **ephemeral** secret, `Content-Type: application/sdp`, the offer SDP as the raw
   body. Returns **`201 Created`** with the answer SDP as the raw body (1633 bytes in
   the observed run for a 1369-byte offer).

The long-lived key never touches the peer connection.

### 3.2 The session object is the format contract

The mint response echoes the negotiated session — this is the authoritative source
for audio formats, and it agrees with what the plan recorded:

```json
"audio": {
  "input":  {"format": {"rate": 24000, "type": "audio/pcm"},
             "turn_detection": {"type": "server_vad", "threshold": 0.5,
                                "prefix_padding_ms": 300, "silence_duration_ms": 500,
                                "create_response": true, "interrupt_response": true}},
  "output": {"format": {"rate": 24000, "type": "audio/pcm"}, "speed": 1.0, "voice": "marin"}
}
```

Default voice is `marin`; `output_modalities` is `["audio"]`; `tools` starts empty
with `tool_choice: "auto"`.

**Opus on the wire, PCM at our boundary.** The negotiated remote track came back as:

```
REMOTE TRACK: track_id=audio kind=Audio ssrcs=[3658811411] codec=Some(("audio/opus", 48000, 2))
```

So: the 24 kHz PCM figures describe the API's own boundary, while the media that
actually crosses the network is Opus at 48 kHz. Our capture path resamples mic audio
to 24 kHz mono for `AudioFormat` purposes. **Resampling therefore happens in
`uia-audio`, once, before the encoder — never in `uia-openai`.**

> **Correction (2026-08-19, S16).** This section originally continued "and the
> WebRTC stack owns the Opus encode/decode". **It does not.** `webrtc` and `rtc`
> are protocol implementations that ship no codec at all: `TrackLocalStaticSample`
> takes already-encoded samples, and the remote track hands back raw RTP. That
> wrong sentence is why no stage owned bringing a codec in, and why S9 shipped a
> `send_audio` that could not send. `uia-audio::codec::OpusCodec` owns both
> the Opus coding and the 24↔48 kHz conversion — see S16's ledger notes for why
> the codec runs at 48 kHz rather than at the boundary rate.

### 3.3 The `webrtc` 0.20.2 API is not the API the plan assumed

This was S1's real unknown, and the answer is that **`webrtc` 0.20 is the sans-IO
rewrite** — a thin async layer over the `rtc` protocol core. None of the 0.11-era
names exist:

| Plan assumed (0.11-era) | Reality in 0.20.2 |
|---|---|
| `webrtc::api::APIBuilder` + `MediaEngine` + interceptor registry | `webrtc::peer_connection::PeerConnectionBuilder`, with `.with_media_engine()`, `.with_interceptor_registry()`, `.with_handler()`, `.with_udp_addrs()` |
| `pc.on_message(...)`, `pc.on_track(...)` callbacks | implement the `PeerConnectionEventHandler` trait (`on_track`, `on_ice_gathering_state_change`, `on_connection_state_change`) and pass it to `.with_handler()` |
| data channel `on_open` / `on_message` callbacks | **poll** it: `dc.poll().await -> Option<DataChannelEvent>` with `OnOpen`, `OnMessage`, `OnClose`, `OnBufferedAmountLow` |
| `pc.gathering_complete_promise()` | no such method — watch `on_ice_gathering_state_change` for `RTCIceGatheringState::Complete` |
| `TrackLocalStaticSample::new(codec_capability, id, stream_id)` | `TrackLocalStaticSample::new(MediaStreamTrack::new(stream_id, track_id, label, RtpCodecKind::Audio, vec![RTCRtpEncodingParameters{…}]))?` |
| `RTCConfiguration { .. }` literal | `RTCConfigurationBuilder::new().with_ice_servers(…).build()` |
| `pc.local_description().await?.sdp` | `RTCSessionDescription` has no public `sdp` field — use `.unmarshal()?.marshal()` |

> **`rtc` must be a direct dependency.** The codec parameter types
> (`RTCRtpCodec`, `RTCRtpCodecParameters`, `RTCRtpEncodingParameters`,
> `RTCRtpCodingParameters`, `RtpCodecKind`) are *not* re-exported by the `webrtc`
> facade, and no local media track can be built without them. `uia-openai` needs
> both `webrtc = "0.20.2"` and `rtc = "0.20.2"`, version-locked together.

Working sequence, verified: build peer connection → register the Opus codec
(payload type 111, 48000/2) → add a sendrecv audio track → `create_data_channel("oai-events")`
→ `create_offer` → `set_local_description` → **wait for ICE gathering `Complete`** →
POST the offer → `set_remote_description(RTCSessionDescription::answer(sdp)?)`.
Observed: `ICE gathering: Complete` → `pc state: connecting` → `pc state: connected`
→ remote track → `data channel open` → `session.created`.

### 3.4 Event vocabulary — the GA names, not the beta ones

**The plan's expected event names are stale.** `response.audio.delta` and
`input_audio_buffer.speech_started` were beta names; the model answers with the GA
vocabulary. Observed verbatim over the `oai-events` data channel:

| Purpose | Event |
|---|---|
| session ready | `session.created`, then `session.updated` after our `session.update` |
| rate limits | `rate_limits.updated` |
| item lifecycle | `conversation.item.added`, `conversation.item.done` |
| response lifecycle | `response.created`, `response.output_item.added`, `response.content_part.added`, `response.content_part.done`, `response.output_item.done`, `response.done` |
| assistant transcript | `response.output_audio_transcript.delta` (43 in one run), `response.output_audio_transcript.done` |
| audio | `response.output_audio.done`; playback bracket `output_audio_buffer.started` (and `.stopped`) — **the audio bytes themselves never appear on the data channel**, they arrive on the RTP media track |
| tool call | `response.function_call_arguments.delta`, `response.function_call_arguments.done` |

The audio row is the structural difference from Nova: over WebRTC there is no
`audio.delta` carrying base64 — audio is media, events are metadata. An engine
implementation that expects audio bytes on the event channel will silently receive
nothing.

Tool call observed verbatim:

```json
{"arguments":"{\"city\":\"Sydney\"}","call_id":"call_YCHPiy0hRxEu1Nxk",
 "item_id":"item_EDeSMI0F7AvxZiniAgCma","name":"get_time","output_index":1,
 "response_id":"resp_EDeSLlsmspc8qbrtGXdwU","type":"response.function_call_arguments.done"}
```

`arguments` is a **stringified** JSON object; `call_id` (not `item_id`) is what the
result must reference. Replying with
`conversation.item.create{item:{type:"function_call_output", call_id, output}}`
followed by `response.create` produced the spoken answer
*"It's 9:20 in Sydney right now."*

### 3.4a Input transcription — the event S1 and S9 could never observe

**Added 2026-08-19 (S16).** Both earlier stages left this open for the same
reason: neither could send microphone audio, so nothing was ever transcribed.
S16 sent real TTS speech through the Opus encoder and captured it.

| Purpose | Event |
|---|---|
| user transcript (final) | `conversation.item.input_audio_transcription.completed` |
| user transcript (partial) | `conversation.item.input_audio_transcription.delta` |

The name is the long `conversation.item.*` form — **not** the
`input_audio_buffer.*` family the plan guessed by symmetry with
`response.output_audio_transcript.*`. The text is in `transcript` on the
completed event and `delta` on the partial. Observed verbatim:

```json
{"type":"conversation.item.input_audio_transcription.completed",
 "event_id":"event_EETUaZ33hQ75xs210nTPr","item_id":"item_EETUXSjP06ErgQk6NUJuo",
 "content_index":0,"transcript":"What is 2 plus 2?",
 "usage":{"type":"duration","seconds":3}}
```

`uia-openai` emits `EngineEvent::UserTranscript` on `.completed` and drops
`.delta`, mirroring the existing one-transcript-per-turn rule for the model
side — the two events carry the same words, so honouring both double-counts
every utterance.

Note also `conversation.item.added` arrives with `"transcript":null` on the
audio content part; the transcript is filled in later by the event above, so
reading it off the item is always empty.

**The remote track streams continuously, including silence.** `AudioChunk`
started arriving before `Ready` and totalled ~25.7 s over a ~33 s session in
which the assistant spoke one sentence. It is a media stream, not a speech
indicator: a HUD (S13) or the A/B harness (S14) must key "assistant is
speaking" off `output_audio_buffer.started`/`.stopped` or `SpeechStarted`,
never off the arrival of audio. `Session` is already correct here — `AudioChunk`
only writes to the sink and drives no state transition.

### 3.5 Tool declarations

Sent after the data channel opens:

```json
{"type":"session.update","session":{"type":"realtime","tool_choice":"auto",
 "tools":[{"type":"function","name":"get_time",
           "description":"Get the current local time for a city.",
           "parameters":{"type":"object","properties":{"city":{"type":"string"}},
                         "required":["city"]}}]}}
```

`parameters` is a real JSON object — contrast Nova's stringified `inputSchema.json`.
`session.updated` comes straight back and echoes the tool list.

---

## 4. What this changes for later stages

1. **S9 must budget for the sans-IO API** (§3.3). The plan's code sketches are
   unusable as written; the tables above are the real call shapes. `rtc` joins
   `webrtc` as a direct dependency — both stay out of `uia-core` under the
   architectural rule.
2. **S9's event parsing must use GA names** (§3.4), and must not expect audio bytes
   on the data channel.
3. **S10 must not wait for `completionEnd`** to end a turn (§2.3), and must queue
   session setup before awaiting `send()` (§2.1).
4. **S2/S7 resampler targets:** 24 kHz for OpenAI, 16 kHz for Nova, both mono 16-bit.
   Nova rejects anything outside {8000, 16000, 24000} with a validation error.
5. **S8's schema translator** has a concrete asymmetry to encode: stringified schema
   for Nova, object for OpenAI (§2.4, §3.5).
6. **Barge-in inputs differ:** Nova pushes `userSpeechStart`/`userSpeechEnd` as
   events; OpenAI handles it server-side via `server_vad` with `interrupt_response`.
   The `Session` state machine sees both through the same `EngineEvent` variant, so
   the port needs a speech-started/stopped pair regardless of provider.

## 5. Reproducing

```bash
# Nova: real speech in, tool round trip, audio out
aws polly synthesize-speech --text "What time is it in Sydney?" --output-format pcm \
    --sample-rate 16000 --voice-id Joanna --engine neural --region ap-southeast-2 say16k.pcm
cd spikes/nova-probe && AWS_PROFILE=sso-nonprod cargo run -- audio /path/to/say16k.pcm 16000
cd spikes/nova-probe && AWS_PROFILE=sso-nonprod cargo run -- audio /path/to/say16k.pcm 44100  # rejection

# OpenAI: WebRTC handshake, events, tool round trip
cd spikes/openai-probe && cargo run          # reads OPENAI_API_KEY, else ../../gpt-api.key
```

Both spikes are excluded from the root workspace (`Cargo.toml` `exclude`), so they
never enter `cargo test --workspace` or CI — they need network and credentials.
