# Voice Assistant — Design

**Date:** 2026-08-16
**Amended:** 2026-08-17 — Gemini Live → OpenAI Realtime (see below)
**Status:** Approved design, in implementation
**Supersedes:** the staged outline in `plans/uia-feature-backlog.md` (retained as the feature backlog)

> **Amendment, 2026-08-17.** Gemini Live is not exposed to developers or the
> public, so the second speech-to-speech provider is **OpenAI Realtime**
> (`gpt-realtime-2.1`) over **native WebRTC**. Nova Sonic is unchanged on HTTP/2
> bidirectional streaming, because AWS documents
> `InvokeModelWithBidirectionalStream` as its sole invocation path — there is no
> WebRTC ingress for the Nova Sonic model itself. A managed alternative does
> exist — **AgentCore Runtime has native WebRTC with KVS-managed TURN** — but it
> terminates the WebRTC leg at a hosted agent that still bridges to the Bedrock
> bidirectional stream, and it mandates a TURN relay hop. It is deferred to
> SP2/SP4, not ruled out. Transport is therefore **per engine** by deliberate
> decision. Separately, the Rust `webrtc` crate
> carries no echo cancellation, so AEC/NS comes from `webrtc-audio-processing`
> on the capture path and benefits both engines.
>
> `.plan/PLAN.md`'s frozen decisions are authoritative where this document and
> it disagree.

## 1. Purpose

A native desktop voice assistant: press a hotkey, speak, get a spoken reply, and
let the model call local tools mid-conversation. It runs as a standalone
conversational agent against two speech-to-speech providers — OpenAI Realtime
and AWS Bedrock Nova Sonic — selected at runtime.

The assistant is **independent of any particular agent platform**. It shares no code,
no schemas, and no SDK with one. An agent platform built in Python can sit behind it;
this assistant is Rust and stays that way. Integration, when it comes, happens through
three optional config-level seams and nothing else:

1. Agents exposed as an **MCP server over HTTP**
2. Optionally pointing at a **shared memory resource** (a config value, not a dependency)
3. Later, A2A / agent cards

No type from any such platform ever appears in this codebase.

## 2. Scope

### Sub-projects

| # | Sub-project | Contents |
|---|---|---|
| **SP1** | **Windows vertical slice** *(this spec)* | hotkey → mic → both engines → speaker → barge-in → one MCP tool |
| SP2 | Memory + settings | first real memory backend; settings UI; secure credential storage; device selection |
| SP3 | HUD polish | glassmorphic layout, audio visualiser, transitions |
| SP4 | Hardening | packaging, latency benchmarks, resource profiling, edge cases |
| SP5 | macOS | second desktop target |
| SP6 | Mobile | iOS + Android: `Activation` adapter, HTTP-only tools, token-exchange credentials |

### Explicitly out of scope for SP1

Settings UI (configuration is a TOML file), the audio visualiser (plain status
text instead), tray menus beyond a quit item, installers, multiple concurrent
MCP servers, wake-word activation, and **any memory backend other than
`NullMemory`**.

### Platform targets

Windows first. macOS, iOS, and Android are later targets that the architecture
must not preclude. Floors inherited from `cpal`: **macOS 14.2+**, **Android API
26+**.

### Development model

Core logic is built and tested headlessly in WSL2; the Tauri shell runs on
Windows for integration testing.

## 3. Architecture

Ports and adapters, with dependency arrows pointing inward. A Cargo workspace,
because that makes "headlessly testable" structurally enforced rather than a
good intention.

```
user-interface-agents/
├── crates/
│   ├── uia-core/     no cpal, no tauri, no network. Defines the ports.
│   │   ├── audio/       PCM conversion, resampling, ring buffer
│   │   ├── engine/      S2sEngine trait + session event types
│   │   ├── session/     conversation state machine (the orchestrator)
│   │   ├── tools/       tool descriptor, call, and result types
│   │   ├── memory/      ConversationMemory trait + NullMemory
│   │   ├── activation/  Activation trait
│   │   └── creds/       CredentialProvider trait
│   ├── uia-openai/   S2sEngine impl — webrtc
│   ├── uia-nova/     S2sEngine impl — aws-sdk-bedrockruntime
│   ├── uia-mcp/      ToolExecutor impl — rmcp, stdio + HTTP
│   ├── uia-audio/    AudioSource/AudioSink impls — cpal
│   └── uia-app/      Tauri: wiring, IPC, tray, hotkey, window
│       ├── desktop.rs   hotkey + tray + overlay window
│       └── mobile.rs    (SP6) push-to-talk + lifecycle + permissions
└── src/                 Svelte 5 — presentation only
```

### Architectural rule (CI-enforced)

> `uia-core`'s `Cargo.toml` MUST NOT depend on `cpal`, `tauri`,
> `tokio-tungstenite`, `webrtc`, `webrtc-audio-processing`, or any `aws-sdk-*`
> crate.

A one-line CI check. It is what stops the core from quietly becoming untestable.

### The seven ports

| Port | Purpose | Production adapter | Test adapter |
|---|---|---|---|
| `AudioSource` | yields PCM frames | `cpal` | WAV fixture reader |
| `AudioSink` | accepts PCM frames; `clear()` for barge-in | `cpal` | in-memory recorder |
| `S2sEngine` | speech-to-speech provider | OpenAI Realtime, Nova Sonic | scripted fake |
| `ToolExecutor` | resolve a tool call | `rmcp` (stdio + HTTP) | fake |
| `Activation` | "user wants to talk now" | global hotkey + tray | channel |
| `CredentialProvider` | supply provider credentials | local store (desktop) | static |
| `ConversationMemory` | cross-session recall | **`NullMemory` only in SP1** | fake |

`Session` in `uia-core` owns the state machine and wires the ports together.

### Why the shell is platform-conditional

Global hotkeys, system trays, and frameless always-on-top transparent overlays
do not exist on iOS or Android. Writing activation logic directly against
`tauri-plugin-global-shortcut` would make the mobile port a rewrite of the app
layer. The `Activation` port keeps `uia-core` from ever learning what
platform it is on; `uia-app` splits behind `#[cfg]` with identical wiring on
both sides.

### Why the core is Rust rather than the WebView

Three reasons, in order of weight:

1. **Nova Sonic is not reachable from a WebView.** It needs SigV4 over an HTTP/2
   bidirectional stream, which browser `fetch`/WebSocket APIs cannot express,
   and doing it in the renderer would put AWS credentials in the web context.
2. **Headless testability.** The chosen dev model requires the core to run under
   `cargo test` in WSL2 with no devices and no window.
3. **Portability.** One Rust core compiles for all five targets. WebView-hosted
   audio would mean reconciling WKWebView against Android System WebView.

The cost is losing `AnalyserNode` for the visualiser; RMS envelopes are computed
in Rust and emitted to the frontend at ~30 Hz instead.

### Frontend boundary

Svelte holds no business logic. Rust emits state transitions, an RMS envelope,
and optional transcript text over Tauri IPC; the frontend sends back `show`,
`hide`, `interrupt`, and `select_engine`. Credentials and audio frames never
cross into the WebView.

## 4. Engine abstraction

The two providers are genuinely different — OpenAI Realtime is JSON events over
a WebRTC data channel with audio on a media track; Nova Sonic is a binary event
stream over HTTP/2 bidi. They do not even share a transport. The trait
normalises the vocabulary they share and nothing else, which is precisely why
the shared contract suite is worth having.

```rust
pub struct AudioFormat { sample_rate_hz: u32, channels: u16, encoding: Encoding }

#[async_trait]
pub trait S2sEngine: Send {
    fn input_format(&self) -> AudioFormat;
    fn output_format(&self) -> AudioFormat;
    async fn connect(&mut self, cfg: &SessionConfig, tx: Sender<EngineEvent>) -> Result<(), EngineError>;
    async fn send_audio(&mut self, frame: &[i16]) -> Result<(), EngineError>;
    async fn send_tool_result(&mut self, id: ToolCallId, r: ToolResult) -> Result<(), EngineError>;
    async fn interrupt(&mut self) -> Result<(), EngineError>;
    async fn close(&mut self) -> Result<(), EngineError>;
}

pub enum EngineEvent {
    Ready, SpeechStarted, AudioChunk(Vec<i16>), SpeechEnded,
    UserTranscript(String), ModelTranscript(String),
    ToolCall { id: ToolCallId, name: String, args: serde_json::Value },
    Interrupted, Error(EngineError), Closed,
}
```

Two deliberate choices:

- **Events leave through an `mpsc::Sender` given at `connect`**, rather than the
  trait returning `impl Stream`. The engine is selected at runtime, so the trait
  must stay `dyn`-compatible.
- **Sample rates are declared by the engine, not hardcoded.** The pipeline
  adapts, and the exact rates are resolved by the verification spike rather than
  guessed here.

### Nova Sonic region constraint

Verified live against the Bedrock control plane on 2026-08-16 from a standard
AWS account:

| Region | `amazon.nova-2-sonic-v1:0` | `amazon.nova-sonic-v1:0` |
|---|---|---|
| us-east-1 (Virginia) | yes | yes |
| us-west-2 (Oregon) | yes | no |
| ap-northeast-1 (Tokyo) | yes | yes |
| **ap-southeast-2 (Sydney)** | **no** | **no** |
| eu-west-1, eu-central-1, ap-south-1 | no | no |

Three consequences, all binding:

- **There is no global endpoint and no inference profile.**
  `list-inference-profiles` returns zero Sonic entries in every region checked.
  `inferenceTypesSupported` is `["ON_DEMAND"]`, so the base model id is invoked
  directly against a region that hosts it.
- **The account's home region cannot serve this model.** Region MUST therefore be
  an explicit `[nova] region` config value, never resolved from the ambient AWS
  profile — otherwise it silently resolves to `ap-southeast-2` and fails with a
  confusing `ValidationException`.
- **Default to `ap-northeast-1`.** From Australia, RTT is roughly 110–130 ms to
  Tokyo against 140–170 ms to Oregon and 200 ms+ to Virginia. This is an
  unavoidable floor on the Nova path's time-to-first-audio.

Access was confirmed already granted in both `ap-northeast-1` and `us-west-2`
(`authorizationStatus: AUTHORIZED`, `entitlementAvailability: AVAILABLE`).

Model modalities are `SPEECH` in, `SPEECH` + `TEXT` out — transcripts are
available natively alongside audio, which is what the memory design in §7
depends on.

**Benchmarking caveat:** OpenAI fronts its APIs with global anycast and is
reached over WebRTC, so it will likely show materially lower RTT from Australia
than Nova-via-Tokyo over HTTP/2. When comparing the two providers, part of the
measured difference is geography and transport rather than model quality.
Latency comparisons must report the region, the transport, and measured RTT
alongside the result.

**Data residency:** conversation audio leaves Australia on the Nova path.
Acceptable for a personal tool, but stated explicitly because conversation audio
is sensitive.

### Engine switching

Dynamic dispatch costs ~1–2 ns per call against a budget dominated by network
RTT and inference; it is not measurable. Requiring an application restart to
change provider was considered and **rejected**: teardown-and-rebuild machinery
is mandatory anyway for backoff reconnection, restart taxes the A/B loop this
project will run constantly, and iOS offers no user-triggerable restart
(calling `exit()` is grounds for App Store rejection).

> **Rule:** engine selection MAY only change while the session is `Idle` — not
> connected, not speaking, no tool in flight.

A one-line guard that removes every hard mid-flight teardown case at no runtime
or portability cost.

### Documented fallback: a pipeline engine (not in SP1)

Verified on 2026-08-16: **Nova Sonic is the only model family on Bedrock that
emits speech.** A modality sweep across us-east-1, us-west-2, and ap-northeast-1
returned only `amazon.nova-2-sonic-v1:0` and `amazon.nova-sonic-v1:0` with
`SPEECH` output. Mistral Voxtral (`voxtral-small-24b`, `voxtral-mini-3b`) takes
`SPEECH`+`TEXT` in but returns `TEXT` only — speech *understanding*, not a
voice. TwelveLabs Marengo is embeddings.

So if Nova Sonic proves inadequate there is **no alternative Bedrock model to
switch to**. Bedrock Marketplace does not help: those models require a dedicated
always-on SageMaker endpoint (~US$1,000/mo for a single `ml.g5.xlarge`), which
is disqualifying for an assistant that idles most of the day, and scaling to
zero reintroduces cold starts that are fatal for realtime voice.

The fallback is therefore a **`PipelineEngine`**: STT → LLM → TTS composed
behind the existing trait. This works because `S2sEngine` is expressed as *audio
in, audio out, plus tool calls* — a pipeline satisfies that contract without any
redesign. It would be a third implementation, not a new abstraction.

| | Native S2S (Nova, OpenAI) | `PipelineEngine` |
|---|---|---|
| Time to first audio | ~300–500 ms | ~800 ms – 1.5 s |
| Reasoning / tool use | limited to the speech model | any LLM, e.g. `global.anthropic.claude-sonnet-5` |
| Voice quality | provider default | best-in-class TTS |
| Paralinguistics, barge-in | native | degraded |

**Trigger for adopting it:** SP1's provider A/B must measure **tool-calling
reliability**, not just latency and voice quality. Tool-call quality is what
determines whether this assistant is useful. If Nova Sonic 2 disappoints there,
the remedy is a pipeline engine driven by a current-generation Claude model —
not a different Bedrock speech model, because none exists.

Out of scope for SP1. Recorded so the option is on the record before the A/B
runs.

## 5. Data flow and state machine

```
AudioSource ─► resample ─► [RMS ─► level event]
                        ├─► barge-in detector
                        └─► engine.send_audio()

engine events ─► AudioChunk ─► resample ─► [RMS] ─► AudioSink
              ├─► ToolCall ──► session ──► ToolExecutor ──► send_tool_result()
              └─► state transitions ─► IPC ─► Svelte HUD
```

Device rate rarely matches engine rate (WASAPI commonly gives 48 kHz), so
`rubato` sits on both legs. This must be real resampling, not 3:1 decimation —
dropping samples without a low-pass filter aliases, and aliased microphone audio
degrades recognition in ways that are miserable to debug later.

**States:** `Idle → Connecting → Listening → Thinking → ToolRunning → Speaking →
Listening`, with `Interrupting` reachable from `Speaking`.

### Barge-in ordering (critical)

When the user talks over the assistant, `AudioSink::clear()` MUST fire
**immediately and locally**, before and independent of `engine.interrupt()`
going over the network. Waiting for provider acknowledgement means the assistant
keeps talking for a full round trip — the thing that makes an assistant feel
broken. This is why `clear()` is a port method rather than buried in the `cpal`
adapter, and it is directly assertable in a headless test.

Tool calls are spawned, not awaited inline: the audio path keeps streaming while
a tool runs.

## 6. MCP bridge

`ToolExecutor` is implemented in `uia-mcp` over `rmcp`, with both transports
from the start: `Stdio` (gated `#[cfg(desktop)]`) and `Http` (all platforms).

Stdio is impossible on iOS — the sandbox forbids subprocess spawning and App
Store review would reject it — and is heavily restricted on Android. **Remote
HTTP MCP is the only viable mobile tool transport**, which is also what makes
an eventual agent-platform integration natural.

**Sequencing constraint:** both providers take tool declarations in their
*session setup* payload, not mid-stream. Therefore:

```
MCP connect → list_tools → translate schemas → build SessionConfig → engine.connect(cfg)
```

Tool discovery strictly precedes engine connection. Schema translation (MCP
`inputSchema` → each provider's function-declaration format) is its own module
with its own tests: it is the piece most likely to silently produce a tool the
model can see but cannot call correctly. Tools are namespaced `server.tool`.

**Safety.** MCP already constrains this well — the model can only invoke tools
from explicitly configured servers and cannot spawn arbitrary commands. A
`requires_confirmation` flag is reserved on the tool descriptor but no
confirmation UI is built in SP1, whose single tool is read-only. This is a
deliberate deferral.

**Timeouts.** A hung tool MUST never hang the session. On deadline expiry the
executor returns an error result, which is sent to the model as a tool result,
and the assistant speaks the failure.

## 7. Memory (port only in SP1)

`ConversationMemory` is deliberately **provider-neutral**. The backend may end
up being AWS AgentCore Memory, Vertex AI Memory Bank, Azure AI Foundry threads,
a local SQLite + embeddings store, or mem0/Zep — so the port carries no
vendor vocabulary.

```rust
pub struct MemoryContext { pub user_id: String, pub conversation_id: String }
pub struct MemoryItem { pub content: String, pub kind: MemoryKind, pub score: Option<f32> }
pub enum MemoryKind { Fact, Preference, Summary, Other(String) }

/// One completed exchange. Populated from transcript events, so both fields are
/// `None` when transcription is disabled.
pub struct Turn { pub user: Option<String>, pub assistant: Option<String> }

#[async_trait]
pub trait ConversationMemory: Send + Sync {
    async fn recall(&self, ctx: &MemoryContext, query: Option<&str>)
        -> Result<Vec<MemoryItem>, MemoryError>;
    async fn record(&self, ctx: &MemoryContext, turn: &Turn)
        -> Result<(), MemoryError>;
}
```

Memory is **orthogonal to the engine** — a memory store is just a store, so any
backend works behind either provider.

**SP1 ships `NullMemory` and nothing else.** No AWS memory dependency.

### "Plugin" means compile-time impls selected by runtime config

Rust has no stable ABI, so `.so`/`.dll` plugins are fragile, and a desktop app
that loads arbitrary code from disk is an attack surface. A config key
(`[memory] backend = "none" | ...`) choosing among compiled-in implementations
gives every practical benefit. Adding a backend is a new crate plus one match
arm.

### Two invariants that must be plumbed in SP1

Retrofitting either is expensive, so both land now even with no backend:

- **Transcript plumbing.** These are *speech*-to-speech engines; memory stores
  text. `UserTranscript` / `ModelTranscript` events and the `SessionConfig`
  transcription flag exist but are **off by default**, since transcription costs
  latency and money and nothing consumes it yet. Enabling any memory backend
  turns it on. Nova Sonic 2 declares output modalities `SPEECH` + `TEXT`, so
  transcripts are natively available (verified 2026-08-16); the OpenAI Realtime
  equivalent is confirmed by spike 3.
- **The recall deadline.** `recall()` runs before `engine.connect()` because
  prior context goes into the setup payload, putting it on the critical path to
  first audio. It gets a **hard ~500 ms deadline; on timeout or error the
  session connects with no memory**. Memory is an enhancement and MUST never
  gate the assistant responding.

`record()` is fire-and-forget on a background task and must never stall a turn.
All memory failures are non-fatal.

### Notes for SP2

- Do not duplicate short-term context: both engines already maintain
  conversation state within a live connection. The value is *long-term,
  cross-session* recall.
- A memory backend could be just another MCP server (mem0 and Zep expose
  them), needing zero new integration code — at the cost of the *model*
  deciding when to remember, which is less reliable than deterministic
  recall-at-connect.
- If AgentCore is chosen: the app gets **data-plane permissions only**
  (`create_event`, `retrieve_memory_records`, `list_events`). The Memory
  resource and its extraction strategies are provisioned via OpenTofu, never by
  the app. KMS encryption at rest with a customer-managed key; minimum viable
  session TTL. This store holds conversation history — people say things to
  voice assistants they would never type.

## 8. Testing strategy

> **Hard gate:** `cargo test` MUST pass in WSL2 with no audio devices and no
> network. Anything requiring either is `#[ignore]`d and run explicitly.

**Layer 1 — unit tests in `uia-core`.** PCM conversion; resampler correctness
(feed a known sine, assert the frequency survives and aliasing does not appear);
RMS; table-driven state transitions.

**Layer 2 — session integration tests in `uia-core`.** The main prize.
`Session` wired to a WAV-fixture source, recording sink, scripted fake engine,
and fake executor exercises the whole product loop deterministically:

- audio reaches the engine; engine chunks reach the sink
- barge-in clears the sink **within 50 ms of detected user speech**, with no
  network involved (the fake engine never acknowledges the interrupt)
- tool call → execute → result → speech resumes
- tool timeout leaves the session alive
- engine drop triggers backoff reconnection
- engine switch rejected while `Speaking`, accepted while `Idle`
- recall timeout degrades to no-memory without delaying connect

**Layer 3 — engine contract suite.** One shared test suite run against both
provider implementations over mock transports, plus a small number of live smoke
tests behind an env var, run manually on Windows.

Manual integration on Windows covers the shell: hotkey, tray, overlay, real
devices. Not automated in SP1.

TDD is required: a failing test comes first, always.

## 9. Error handling

| Failure | Behaviour |
|---|---|
| Auth (bad key, no model access) | Terminal for that engine. Distinct HUD state. **No retry loop** — this is the most-misdiagnosed failure and a backoff spinner hides it |
| Network drop | Exponential backoff: 500 ms base, ×2, 30 s cap, jitter. State → `Connecting` |
| Tool error / timeout | Non-fatal. Becomes a tool result the model speaks |
| Memory error / timeout | Non-fatal. Degrade to no-memory; subtle HUD indicator |
| Audio device lost | Re-enumerate, fall back to default, surface to HUD |
| Protocol / model error | Log raw frame, generic surface |

Typed errors per crate via `thiserror`, mapped to `EngineError` / `ToolError` /
`MemoryError` at the boundaries.

## 10. Verification spikes (before any provider code)

1. ~~**Nova Sonic access and model id.**~~ **RESOLVED 2026-08-16.** Model id is
   `amazon.nova-2-sonic-v1:0`; access is already `AUTHORIZED` with entitlement
   available in `ap-northeast-1` and `us-west-2`. No use-case form required. See
   the region constraint in §4 — this is no longer a blocker.
2. **Nova Sonic bidi event shape and audio formats**, via
   `invoke_model_with_bidirectional_stream` against `ap-northeast-1`. Fills in
   the `AudioFormat` values this spec deliberately leaves unresolved. **Still
   the highest-risk unknown** — the AWS CLI cannot exercise bidirectional
   streaming, so this needs a small Rust probe.
3. ~~**A Gemini Live API key and a successful handshake.**~~ **SUPERSEDED
   2026-08-17** — Gemini Live is not available to developers. Replaced by: **an
   OpenAI Realtime WebRTC session from Rust.** The account, key, model id
   (`gpt-realtime-2.1`), audio formats (PCM 24 kHz both directions), and the
   SDP and ephemeral-secret endpoints are already verified; the open risk is the
   `webrtc` crate's SDP offer/answer and ICE handling from Rust.
4. **Tauri + Svelte scaffold** building and launching on Windows.

Spike 2 is now the one that can most change the plan, since the entire audio
pipeline is shaped by the formats it returns.

## 11. Dependencies

Latest stable at time of writing; re-verify against the registry before pinning.

| Crate / package | Version | Role |
|---|---|---|
| `tauri` | 2.11.5 | desktop + mobile shell |
| `tauri-plugin-global-shortcut` | 2.3.2 | hotkey (desktop only) |
| `tauri-plugin-store` | 2.4.4 | local config/credential store |
| `svelte` | 5.56.9 | frontend |
| `@tauri-apps/api` | 2.11.1 | frontend bindings |
| `cpal` | 0.18.1 | audio I/O — Windows/macOS/iOS/Android |
| `rubato` | 5.0.0 | resampling |
| `tokio` | 1.53.1 | async runtime |
| `webrtc` | 0.20.2 | OpenAI Realtime transport (protocol only — no AEC) |
| `webrtc-audio-processing` | 2.1.0 | capture-side echo cancellation / noise suppression |
| `aws-sdk-bedrockruntime` | 1.139.0 | Nova Sonic (`invoke_model_with_bidirectional_stream`) |
| `aws-config` | 1.10.1 | credential resolution |
| `rmcp` | 3.1.2 | MCP client |
| `async-trait` | 0.1.92 | `dyn`-compatible async traits |
| `thiserror` | 2.0.20 | typed errors |

Deferred to SP2 if AgentCore is chosen: `aws-sdk-bedrockagentcore` 1.59.0 (data
plane) and `aws-sdk-bedrockagentcorecontrol` 1.72.0 (control plane). Both AgentCore
endpoints were confirmed present in `ap-southeast-2`; account-level access remains
unverified.

## 12. Success criteria for SP1

1. Pressing the hotkey shows the overlay; speaking produces a spoken reply.
2. Both OpenAI Realtime and Nova Sonic work, selectable while `Idle`.
3. Barge-in halts playback locally, without waiting on the network.
4. The model calls one local MCP tool and speaks the result.
5. A dropped connection reconnects with backoff.
6. `cargo test` passes in WSL2 with no devices and no network.
7. The provider A/B records **tool-calling reliability** for each engine, not
   just latency and voice quality, and reports the Nova region and measured RTT
   alongside the numbers. This is the evidence that decides whether the
   `PipelineEngine` fallback in §4 gets built.
