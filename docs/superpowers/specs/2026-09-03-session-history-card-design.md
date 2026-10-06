# Session history card (and the transcription switch that makes it possible)

## Problem

The HUD's typed-message log (`HudCard.svelte`, `sentLog`) holds the last
five things the user **typed**, in frontend state only, discarded on
every webview reload. It shows neither spoken turns nor any assistant
reply. Now that the HUD has grown to 342x213 there is room to do better,
and the user wants a way to read back the conversation.

The machinery to do so already exists end to end, but the feed into it
is switched off:

1. `Session::handle_event` accumulates both halves of an exchange —
   `EngineEvent::UserTranscript` into `pending_turn.user`,
   `EngineEvent::ModelTranscript` into `pending_turn.assistant` — and
   `flush_turn()` writes the completed `Turn` to memory on every
   `SpeechEnded` (`crates/uia-core/src/session/mod.rs`).
2. `FileMemory` persists those turns as newline-delimited JSON,
   retaining the last `RETAIN_LIMIT` (200) exchanges verbatim
   (`crates/uia-app/src/memory.rs`).
3. But `SessionConfig::transcription` is `false` by default and **is
   never assigned `true` anywhere outside tests**. There is no config
   surface for it.

That flag gates only the **user's** half. In `uia-openai` it controls
`audio.input.transcription`, so `conversation.item.input_audio_transcription.completed`
— and therefore `UserTranscript` — never arrives. The assistant's half
is not gated at all: `ModelTranscript` comes from
`response.output_audio_transcript.done`, which the engine emits as a
normal part of any audio response.

The result is a lopsided record. Confirmed against a real store
(`uia-conversation.jsonl`, 8 recorded exchanges): **every entry carries
an `assistant` key and not one carries a `user` key.** Memory is
recording everything the assistant said and nothing the user said, and
recall replays exactly that.

Those two transcript events are the only writers to `pending_turn` —
typed turns do not touch it, since `pump_text` calls
`engine.send_text()` and returns.

This has a consequence beyond the history feature. The General tab's
"Remember the conversation" switch already tells the user:

> Keeps a transcript in `uia-conversation.jsonl` next to your config,
> and replays recent exchanges when the session reconnects.

On OpenAI and Foundry that is half true in the least useful direction:
the assistant recalls its own words but never the user's. Only Nova,
whose protocol parses `USER`/`ASSISTANT` content unconditionally,
records complete exchanges today.

So this spec does two things: it makes memory actually record, and it
adds a surface for reading what was recorded.

## Non-goals

- **Live-updating history.** Turns land in memory one entry per completed
  exchange. The card reads the store when opened and on an explicit
  refresh; it does not stream a partial exchange in progress.

  *As built:* this holds for the card, and stopped holding for the HUD.
  The strip the card opens from is live, fed by a `uia://turn` event
  emitted where the turn is recorded. That was not in this spec and was
  added when the strip became the way in: a strip showing only what the
  user had *typed* was empty for anyone who speaks to the app, which is
  everyone. The card stays a snapshot, because reading back a transcript
  and watching one accumulate are different jobs.
- **Search, filter, or export.** A readable scrollback, nothing more.
- **Editing or deleting individual turns.** Clearing history is
  out of scope here; the file is user-owned and already deletable by
  hand.
- **Raising `RETAIN_LIMIT`.** 200 exchanges is what the store keeps; the
  card reports that bound rather than changing it.
- **Captioning replies live in the HUD.** A separate concern from
  reading back a stored transcript.

## Decision: transcription follows memory

`transcription` is set from the same resolved value that decides whether
memory runs at all — one switch, not two.

The alternative considered was a second toggle nested under "Remember
the conversation" ("also enable transcript, if available"). Rejected:
its *off* position is exactly today's silently-broken configuration,
where memory is enabled and records only the assistant's half of every
exchange. A switch whose off state leaves its parent feature recording a
one-sided conversation is a trap rather than a choice.

The cost is real and must be disclosed rather than hidden — the engine's
own comment is "transcription costs latency and money, and nothing
consumes it until a memory backend is enabled". The second half of that
sentence stops being true with this change, and the first half moves
into the UI:

> Keeps a transcript in `uia-conversation.jsonl` next to your config,
> and replays recent exchanges when the session reconnects. Nothing
> leaves this machine. Recording your own words means asking the engine
> to transcribe them, which adds a little latency and cost to each
> exchange.

Behaviour change to state plainly: anyone who already has memory enabled
starts paying for input transcription after this lands. What they get
for it is the half of the conversation that has been missing — their
own words, both in the stored history and in what the assistant recalls
on reconnect. That is a real improvement rather than a new cost for
nothing, but it is a change, and the release notes should say so.

## Per-engine reality

| Engine | User half | Assistant half | After this change |
|---|---|---|---|
| OpenAI | gated on `cfg.transcription`, never arrives | recorded already | complete exchanges |
| Foundry | same gate, via the same code | recorded already | complete exchanges |
| Nova | recorded | recorded | unchanged |

Foundry is not a special case. It has no protocol module of its own: it
imports `uia_openai::protocol`'s `parse_server_event` and
`session_update` wholesale, so `cfg.transcription` reaches
`audio.input.transcription` there by exactly the same path. Grepping
`crates/uia-foundry/src` for the transcript event names finds nothing
and means nothing — the parsing lives in the crate it borrows.

So every engine records complete exchanges once this lands, and the card
needs only one empty state: nothing recorded yet.

**Existing stores are half-populated and stay that way.** Every exchange
already on disk has `user: None`, and nothing can backfill it. The card
renders a turn with a missing half as exactly that — the assistant's
line with no matching user line — rather than hiding it or inventing a
placeholder. Users with memory already enabled will see a boundary in
their history where the user's side starts appearing.

## Reading the store back

`FileMemory` lives inside the session, built by `build_session` from a
`memory_path`; `uia-app` itself only holds that `Option<PathBuf>`. The
store is file-backed and appended on every completed turn, so a command
can read it from disk without reaching into the session.

`Memory::recall` is not usable for this: it caps at `RECALL_LIMIT` (20)
and flattens each `Turn` into separate `MemoryItem`s, losing the
user/assistant pairing the card needs.

New pieces:

- `FileMemory::turns()` — returns the retained `StoredTurn`s, newest
  last, preserving pairing. Reads the in-memory mirror under the
  existing mutex.
- `memory_path` added to Tauri managed state.
- `get_history` command — loads the store from disk and returns
  `Vec<{ user: Option<String>, assistant: Option<String> }>`, plus
  whether memory is enabled at all, so the card can tell "turned off"
  apart from "nothing recorded yet".

Reading from disk rather than the live session means an exchange still
in progress is absent until it flushes. That is correct for a
"conversation so far" view and avoids a second source of truth.

## The card

A third layer kind in `CardDeck`, alongside `hud` and `settings`, closed
like Settings and reusing the deck's existing sizing, genie animation and
close path.

*As built:* opened from the conversation strip above the text input, not
from a chip on the top row. The strip is already the thing on screen that
means "what has been said", and the card is the same idea at length —
whereas a fifth chip would have sat among the window controls saying
nothing about itself. `settingsOpen: boolean` became
`overlay: 'settings' | 'history' | null`, so the deck holds one overlay at
a time by construction and its depth arithmetic stays a two-layer problem.

Settings drops the engine connection while open; history does not. The
drop exists for rebuild-the-session changes — engine, model, key. Reading
a transcript changes nothing and is usually a glance, and a reconnect
costs about a second of not being able to talk.

- Size: 600x584, matching `settings`.
- Content: one row per exchange, `You` / `Agent` label plus the text,
  newest at the bottom, scrolled to the bottom on open.
- Footer: states the 200-exchange bound honestly — "Showing the last 200
  exchanges; older entries are dropped as the log compacts."
- Empty states, distinct: memory disabled, versus memory on with
  nothing recorded yet.

### The genie anchor

`transform-origin` for the settings panel is derived from where the cog
sits in the HUD. This spec anticipated a history chip on the same row,
which would have shifted every chip left of it and forced the settings
anchor to be re-derived in the same change.

*As built:* no chip was added, so nothing on that row moved and the
settings anchor is untouched. The history card grows from the strip
instead — `58% 47%`, derived in `CardDeck.svelte` from the strip's laid-out
box. That origin is approximate in a way the cog's is not, because the
strip is flex-sized rather than at a fixed offset; the slack in the input
row's height moves it by well under a percent, which is why an
approximation is good enough here and would not have been for the cog.

## Testing

- `FileMemory::turns()` — pairing preserved; ordering; behaviour across
  a compaction; empty and missing file.
- `transcription` resolves from the memory setting — on when memory is
  on, off when off.
- `uia-openai`: `transcription_is_requested_when_configured` already
  covers the protocol side; assert the wiring reaches it from the
  resolved setting.
- `get_history` — returns pairs, reports the disabled and
  cannot-transcribe cases distinctly.
- Frontend: `pnpm check`, then a live run confirming an OpenAI exchange
  reaches `uia-conversation.jsonl` and appears in the card.

## Staging

1. **Prove the pipeline.** Partly done: the store on disk already holds
   assistant-only exchanges, which proves `flush_turn`, `FileMemory` and
   the append path all work end to end. What remains unproven is the
   user half — that setting `transcription = true` actually produces
   `UserTranscript` from a live OpenAI session. Confirm that before
   building the card on the assumption of paired turns.
2. **Transcription follows memory**, with the General tab hint updated.
   Ships on its own: it makes memory work, with or without the card.
3. **`FileMemory::turns()` + `get_history`.**
4. **The history card and deck plumbing.** Done, without the chip — see
   "The card" above for what replaced it.

Stage 2 is independently valuable and independently releasable, which is
the argument for this order.

## Open questions

- Should the history card offer "clear history"? Out of scope above, but
  a persisted transcript with no in-app delete is arguably incomplete.
  **Still open.** The file remains user-owned and deletable by hand.
- ~~Should the chip be hidden entirely when memory is disabled, or shown
  and explain itself?~~ **Moot.** There is no chip. The strip is always
  present, and the card carries two distinct empty states — memory off,
  versus memory on with nothing recorded yet — which is what `get_history`
  reports `enabled` separately for.

## Known gaps

- A one-sided exchange renders as exactly that: the assistant's line with
  no user line above it. Everything recorded before transcription was
  enabled is that shape and nothing can backfill it, so users with memory
  already on will see a boundary in their history where their own side
  starts appearing. This is by design, and the same is true of the live
  strip, so the two never disagree.
- The strip caps at the last few exchanges and is frontend-only, discarded
  on a webview reload. `uia-conversation.jsonl` is the record; the strip is
  a trace of the session on screen.
