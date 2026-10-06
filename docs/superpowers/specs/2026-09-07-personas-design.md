# Personas: structured identities, and letting the assistant switch between them

## Problem

The General tab gives the assistant exactly one identity: a name, an
avatar image, and a persona document. The persona is free prose the user
edits as a whole (`get_agent_persona`/`set_agent_persona`), stored at a
path the app computes itself — `agent_persona_path_for` pins it to
`uia-agent.md` beside `uia.toml`, and the field is round-tripped
untouched through the settings boundary so no user-supplied path ever
reaches the filesystem (`crates/uia-app/src/settings.rs`).

The path discipline is sound and this spec keeps it. Three things about
the rest do not survive.

1. **It holds one identity.** There is one `uia-agent.md`. Adopting a new
   persona means pasting over the only copy; the previous one is gone,
   with no history and no way back.
2. **Changing it is a three-field edit.** Name, prose and avatar are
   independent controls in `SettingsGeneral.svelte`. "Switch to secretary
   mode" means editing all three consistently by hand and getting it
   right, in a settings tab, in an app whose whole premise is that you
   are not looking at it.
3. **An empty textarea guides nobody.** A blank box labelled "Persona"
   tells the user nothing about what makes a persona work on a *realtime
   voice* model. People write who the assistant is and omit how it should
   sound, which is the half that actually changes the experience.

The need is situational identity: a secretary during work, a friendly
one outside it, a terse one in focus mode — selected by asking out loud,
not by opening Settings. A voice-first assistant that requires a settings
visit to change how it speaks has the feature in the wrong modality.

## Non-goals

Deliberately excluded, each for a stated reason:

- **Per-persona voice, engine or model.** Tempting — "research mode runs
  Nova" — but engine choice is restart-scoped and load-bearing for
  audio-format negotiation and `set_max_session_duration`'s per-provider
  ceiling. Coupling it to a mid-conversation switch multiplies the blast
  radius of a feature whose value is behavioural. Revisit separately.
- **Per-persona MCP server sets.** This would put personas inside the
  tool-trust boundary that `docs/MCP.md` deliberately drew: approval is
  per server, recorded in `uia-mcp.json`, and enabling one is "the whole
  trust decision". A persona that silently enables servers moves that
  decision somewhere the user is not looking. Not now, and not without
  its own design.
- **Installable or shareable personas, and any marketplace.** A persona
  is a handful of fields the user wrote. Fetching them from elsewhere
  reintroduces arbitrary third-party content into a process holding a
  live microphone, and adds a supply-chain surface to a repo about to go
  public. The MCP seam already exists for genuine capability extension.
- **A raw system-prompt override.** Considered and rejected — see
  "Composition" below.
- **Autonomous switching.** See "Who decides".
- **Live `session.update` prompt swap.** Deferred, with reasoning, under
  "Why rotation and not a live swap".

## Who decides

The assistant may **suggest** a switch and must **ask** before making
one. The user confirms, and only then does the tool fire.

This is best-effort, not enforced, and the spec says so plainly rather
than pretending otherwise. The confirmation lives in the system prompt,
and a realtime speech model will sometimes skip it — instruction
adherence under load is exactly what these models are weakest at, and
`system_prompt_for` already carries `BREVITY_INSTRUCTION` and
`NO_SELF_INTRODUCTION` for the same reason.

Hard enforcement was considered and rejected. A confirm dialog drags a
click into a hands-free flow, which is the cost the feature exists to
remove. The failure it would prevent is low-harm: a wrong persona is
immediately obvious, affects only tone, and is one sentence to undo. So
the mitigation is **loud and reversible** instead of enforced:

- Every switch is announced by the assistant as it happens.
- `switch_persona` accepts the literal id `previous`, so "switch back"
  needs no name recall.

A user who does not want any of this leaves `allow_agent_switch` off,
and the tool is never declared at all.

## What a persona is made of

A persona is structured fields, not free prose. The structure exists to
guide — a blank box produces personas that describe character and forget
delivery — and to make composition deterministic.

Every field must serve one of exactly two consumers, and this is the
test for whether a proposed field belongs:

- **The selection surface** — what the model sees about a persona it is
  *not* using, carried in the tool descriptor for the whole session.
  One line, and only `use_when` qualifies.
- **The active prompt** — composed into `instructions` only while that
  persona is live.

### Required

| Field | Purpose |
|---|---|
| `label` | What the user calls it — "Work", "Focus". UI identity, and the source of the id. |
| `archetype` | One noun phrase: "executive secretary", "patient tutor". The highest-leverage field per token: a realtime speech model follows long instructions poorly but carries a large behavioural prior on a strong role noun, so an archetype does more work, more reliably, than three sentences of description. |
| `manner` | How it *speaks* — sentence length, formality, warmth, whether it acknowledges before answering. The field that matters most for a voice agent and the one free-text personas reliably under-specify. |
| `use_when` | The situation this persona is for. The only thing the model sees about an inactive persona, so it describes the *situation*, never the voice. Load-bearing only while `allow_agent_switch` is on, but collected always so turning the toggle on needs no second pass. |

There is deliberately no separate "personality description" field.
`archetype` and `manner` cover it with sharper prompts, and a third
overlapping prose field is where the same content gets written twice.

### Optional

| Field | Why not required |
|---|---|
| `boundaries` | Hard limits as short lines — "never commit to anything on my behalf". Often the most valuable field in practice, but genuinely empty for casual personas, and requiring it would produce filler. |
| `extra` | Free-prose remainder. Structure that cannot express something makes users fight the form; a remainder field means the fields guide without caging. |
| `name_override` | The assistant's spoken name while this persona is active. Blank means the global `[agent] name` stands, which is the expected case: most users want one assistant that behaves differently, not several assistants. |
| `avatar_ext` | Set by the app when an avatar is chosen, not typed. |

## Storage and schema

Personas move out of `uia.toml` into a sidecar, `uia-personas.json`
beside it. This follows the `uia-<thing>.json` convention `uia-mcp.json`
already established, including holding its own global gate the way that
file holds `allow_remote`.

The separate-file rule that governs the current persona
(`agent_persona_path_for`, justified in its own comment because "a
persona is prose a user edits as a whole document, not a scalar") does
not survive the restructure: short structured fields are exactly the
scalars that justification excluded. One sidecar, not a file per
persona.

```json
{
  "active": "default",
  "allow_agent_switch": false,
  "personas": [
    {
      "id": "default",
      "label": "Default",
      "archetype": "general-purpose assistant",
      "manner": "Plain and direct. Short sentences. Answers first.",
      "use_when": "Anything that does not call for a more specific persona.",
      "boundaries": "",
      "extra": "",
      "name_override": "",
      "enabled": true
    }
  ]
}
```

`[agent] name` stays in `uia.toml` as the global assistant name.
`persona_path` and `avatar_path` are removed from `AgentSettings`.

Avatars remain files, since they are binary:
`uia-agent-avatar-<id>.<ext>`, app-derived from the id.
`copy_agent_avatar`'s extension allow-list and stale-file cleanup apply
unchanged, per id. Ids are validated at creation — no path separators,
no traversal sequences — so an id never reaches the filesystem
unchecked.

**At most six personas.** The cap is enforced when the sidecar loads,
not only in the UI: it exists for the model's benefit as much as the
user's, and a hand-edited file with twenty personas would put twenty
`use_when` lines in every session's tool descriptor. A sidecar over the
limit loads the first six and reports the rest as dropped, rather than
failing outright.

`enabled` is per-persona, so a user can keep one without exposing it to
the model. With the global toggle that gives the two controls asked for:
leave the feature off entirely, or expose only the personas wanted.

### No migration

There are no existing installs, so `persona_path` and `avatar_path` are
**replaced** rather than kept alongside. A single `default` persona is
created on first run and there is no legacy shape to read.

Worth stating rather than leaving implicit: had there been users,
adopting `uia-agent.md` in place would have been the compatible move,
and a later change that does have users to protect should not read this
section as a precedent for a clean break.

## Composition

`system_prompt_for` no longer receives prose to pass through. It builds
the prompt from the active persona's fields in a fixed order:

```
Your name is {name_override or global name}.
You are {archetype}.
{manner}
{boundaries}
{extra}
{BREVITY_INSTRUCTION} {NO_SELF_INTRODUCTION}
[{switch clause, only when allow_agent_switch}]
```

Empty optional fields contribute nothing — no blank lines, no dangling
connective text.

This removes today's persona-replaces-the-whole-prompt behaviour, and
with it the empty-string suppression, which existed only to let a
persona blank the default prompt entirely.

That loss is the point rather than a side effect. A raw override is the
one path that can silently drop `BREVITY_INSTRUCTION` and
`NO_SELF_INTRODUCTION` — the two rules that keep a voice assistant from
monologuing and from re-introducing itself every turn. Users reach for a
raw override to add something, not to delete those, and `extra` lets
them add without the deletion. An advanced raw-override mode was
considered and rejected: it means two shapes through composition, and
the second one is untested in practice precisely because it is the
advanced path.

The switch clause tells the model it may suggest a switch when the
situation calls for it, must ask first, must say so afterwards, and may
call `previous` to undo. With the toggle off the clause is absent
entirely, so a user who does not want the feature pays no prompt tokens
for it.

## The tool

`switch_persona(id)` is a built-in, and this is the first one. Note that
`clock_tool()` in `uia-engine-contract` is **not** a precedent — it is a
test-harness fixture, deliberately dotted to exercise `uia-mcp`'s
wire-name sanitisation. In production every tool reaching the session
comes from `build_executor`, which fans the approved MCP servers out
behind one `Arc<dyn ToolExecutor>`.

So the built-in arrives as a **decorator** over that executor: it
answers `list_tools` with `switch_persona` prepended to whatever the MCP
layer declares, handles `execute` for that one name, and delegates
everything else untouched. `ToolExecutor` is a two-method trait, which
makes this a small and honest seam; MCP dispatch is not modified at all.

The tool is declared only when `allow_agent_switch` is on **and** at
least two personas are enabled. One persona means there is nothing to
switch to, and declaring a tool that can only fail is worse than
declaring none.

Its descriptor enumerates enabled personas as an enum of ids, each
carrying its `use_when` as the description — and nothing else. Ten
personas cost ten short lines of context, not ten prompts.

## Applying the switch: rotation, not reconnect

The session already knows how to replace its own connection
mid-conversation. `Session::run` checks `session_too_old()` and, when
due, runs `flush_turn(); disconnect(); connect();` — rotating ahead of
the provider's ceiling specifically so that "the conversation survives
it" (`crates/uia-core/src/session/mod.rs`). The conversation survives
because `connect()` replays prior turns as real attributed conversation
items via `memory_item_events`, rather than pasting a wall of quoted
text into the instructions.

A persona switch is that same rotation with a different prompt. The
implementation is therefore not new machinery but one more reason to
rotate, checked beside the existing one in the run loop:

1. The tool call sets a pending-switch request on the session's control
   surface and returns its `ToolResult` immediately. It does **not** tear
   down the session from inside its own event loop — the executor is
   called from `handle_event`, and reconnecting underneath that is
   re-entrant.
2. The run loop sees the pending request at the same point it checks
   `session_too_old()`, calls `set_system_prompt` with the newly composed
   prompt, and rotates.
3. `connect()` replays history as it already does. The assistant
   announces the change on the other side.

`active` is persisted only **after** the reconnect succeeds, so a crash
or a failed connect mid-switch leaves the user on the persona they had,
not on a half-applied one.

### Error handling

- **Unknown, disabled, or malformed id** — the tool returns
  `ToolResult::error` and sets no pending switch. Nothing rotates. The
  model gets a normal tool error and can tell the user.
- **Reconnect fails** — restore the previous persona's prompt and
  connect with it. This costs nothing extra: it is the same rotation
  path, run once more with the old value.
- **The retry also fails** — fall through to the existing session-error
  handling. A persona switch must not invent a new terminal state.
- **An entry missing a required field** loads as disabled and is
  reported in the Personas tab, rather than failing the whole sidecar.
  One bad entry must not cost the user their other personas.
- **The sidecar is absent or unreadable** — start from a single default
  persona, as on first run. Never start with no identity at all.

### Why rotation and not a live swap

`session_update` sets `instructions` in a re-sendable event, so on
OpenAI and Foundry a live prompt swap is plausible — seamless, with no
audible gap. It is nonetheless not the design here, for two reasons.

Nova cannot do it. `uia-nova`'s system prompt is a `SYSTEM` content
block sent at connect time and marked in its own source as "setup, not
interactive". There is no live-swap path on that engine, so a live-swap
design needs the rotation path anyway as a fallback — meaning two
divergent routes through identity, one of them exercised only on a
non-default engine, which is where bugs go to hide.

And the OpenAI half is unverified. `instructions` are documented to
apply to subsequent responses, but nobody here has watched a mid-session
swap take effect, and this codebase has a standing rule about that:
`TRANSCRIPTION_MODEL` carries an explicit UNVERIFIED marker for exactly
this class of assumption.

Rotation is one path, identical on all three engines, built from
mechanisms already in production. The live swap is a legitimate
follow-up optimisation once the rotation gap has been measured in real
conversation — not a prerequisite.

## Settings UI: a frame in front of General

Personas do **not** get their own tab, and do not get their own window.
General keeps its persona slot, but the free-text field is replaced by a
**Personas** button that opens a frame standing in front of the General
panel. The avatar picker leaves General too, since an avatar now belongs
to a persona rather than to the app.

### Why not a bigger window

There is one window. `CardDeck` resizes it per card —
`hudMini` 342x58, `hudFull` 342x213, `settings` 600x584 — and `history`
takes the settings footprint on purpose, for a reason the deck states
itself: sharing it "means opening either from the HUD is the same window
movement, and the deck never resizes when moving between them."

A `personas` overlay kind with its own larger size would be the first
thing to break that rule, and the resize would fire from a button
*inside* the settings card, which is the worst place for the window to
jump. So Personas is not a new overlay: it is a step within the existing
one, at the existing size.

The height is sufficient without argument from taste. `settings`'
584px floor was set by the MCP table's column headers, with unbounded
server names and launch paths that wrap and scroll inside themselves. At
most six personas is a strictly smaller problem than that.

### The frame

It follows `SettingsMcp`'s install flow exactly — where `null` is the
list and anything else is "the install flow standing in front of it" —
so the interaction is one the user has already met in this app:

- **List step** — the personas, each with its label, enabled state, and
  which is active. Add, duplicate, delete, select active.
  Duplicate matters more than it looks: the fastest way to write a
  second persona is to edit a copy of one that works.
- **Editor step** — the four required fields, each with a placeholder
  that demonstrates rather than describes ("executive secretary" beats
  "the role this persona plays"), and the optional fields behind a
  collapsed "More" so the common case is four inputs, not seven.

Steps replace each other rather than sitting side by side. At 600px
wide a master-detail split leaves roughly 280px per pane, too narrow for
the `manner` field, which is the one most likely to run to several
lines.

**Validation:** a persona with an empty required field cannot be
enabled, and says which field is missing. It can still be saved:
half-written personas are normal, and losing work to a validation gate
is worse than holding a disabled draft.

Selecting the active persona in Settings stays **restart-to-apply**, as
identity changes are today. Only the voice path rotates; making the
Settings selection rotate a live session is a separate change and is not
proposed here. This needs no extra explanation in the UI — the tab
already shows its restart notice exactly on the settings that require
one, so the behaviour is self-documenting.

The `allow_agent_switch` toggle does need its own wording: it should say
plainly that the assistant may suggest switching and will ask first, and
that asking is best-effort rather than enforced.

## Testing

TDD, failing test first. All of this is unit-testable offline; none of
it requires a live engine.

- Sidecar load: absent file yields one `default` persona, active and
  enabled, and round-trips through save.
- Sidecar load: an entry missing a required field loads disabled, and
  the other personas survive.
- Sidecar load: a file holding more than six personas keeps the first
  six and reports the rest, rather than failing the load.
- Composition: each field appears in the fixed order; empty optional
  fields contribute nothing, leaving no blank lines or dangling
  connectives; `name_override` wins over the global name when set and
  defers to it when blank.
- Composition: `BREVITY_INSTRUCTION` and `NO_SELF_INTRODUCTION` are
  present for **every** persona — there is no field combination that
  drops them.
- Prompt: the switch clause is present with the toggle on and absent
  with it off.
- Descriptor matrix: `switch_persona` is declared only for toggle-on and
  two-or-more enabled personas; absent for toggle-off, for one enabled
  persona, and for zero. Each entry carries its `use_when` and no other
  field.
- Executor decoration: `switch_persona` is answered by the built-in, and
  every other name delegates to the wrapped executor unchanged.
- Id resolution: unknown, disabled and malformed ids return an error and
  set no pending switch; `previous` resolves to the prior persona, and
  to an error when there is no prior persona.
- Rotation: a pending switch causes exactly one rotation, sets the new
  prompt before `connect()`, and persists `active` only after the
  connect succeeds. A failing connect restores the previous persona.
- Path safety: an id containing a path separator or traversal sequence
  is rejected at creation, never reaching the filesystem.

## Open questions

- ~~The rotation gap is a risk, not a known defect.~~ **Measured
  (2026-09-09): median 842 ms, and the live-swap follow-up is not
  triggered.** Five switches on OpenAI mid-conversation, ten exchanges of
  history replayed each time, via `crates/uia-app/tests/rotation_gap.rs`
  (`#[ignore]`d; needs `OPENAI_API_KEY`). Rotation took 812 ms, 843 ms,
  1.02 s, 839 ms and 2.23 s — median 842 ms, four of five under the 1.5 s
  "reads as a pause" bar, none within a second of the 3 s "reads as a
  fault" bar that would have opened the live-`session.update` work. So
  rotation ships as designed, and the follow-up stays a genuine
  optimisation rather than a correction.

  The first turn after a switch cost **1.81–2.46 s against a 1.77 s
  no-switch baseline** — i.e. within noise. Replaying 2180 characters of
  history into a fresh session does not measurably slow its first
  response, so the entire user-visible cost of a switch is the reconnect,
  and the history term the live swap would save is worth close to nothing
  at this conversation length. That is the strongest argument yet that the
  follow-up is low-value: it would buy back the 842 ms, not the 2.6 s a
  user actually waits through.

  Three caveats on the number. It is a **floor**: the harness runs a
  `FakeExecutor`, so it excludes `list_tools()`, which `Session::connect`
  awaits with no deadline and which `McpRouter` walks sequentially across
  servers — one slow MCP server would land on top of every figure here
  (worth its own fix; see the ledger). The 2.23 s outlier is one sample in
  five and uncharacterised — if the tail matters, the harness takes a
  larger `ROUNDS`. And it is OpenAI only; Foundry and Nova are unmeasured,
  though Nova has no live-swap path to choose anyway.

- ~~The gap is a deaf window, not a pause — and the UI does not say so.~~
  **Settled: signalled, not accepted.** Found while measuring, and separate
  from whether 842 ms is acceptable. `apply_pending_prompt_swap` disconnects
  before it reconnects, so for the whole gap the session is not listening:
  speech into it is discarded, not buffered. The bar above judges the gap as
  silence a user waits through, but a user who answers into that window loses
  the turn and got no indication they did.

  The window is now stated rather than inferred. `hud::can_hear` is false for
  exactly `Idle` and `Connecting` — the two states in which `Session::events`
  is `None` — and rides on the existing `uia://state` event as a `hearing`
  field, so the state and the answer to "can it hear me" arrive together and
  cannot drift apart. That needed nothing new on the rotation path: rotation
  already moves through both states, so this covers the persona switch, the
  session-duration rotation, every reconnect and the idle disconnect at once.
  The HUD reads it as a status line naming the consequence rather than the
  cause (`Connecting — not listening`) and a hollow indicator dot, kept
  distinct from the filled-grey muted one because only one of the two is the
  user's own doing and only one clears itself.

  Measuring the fix turned up a second half this question had not seen:
  `disconnect()` left the level meter standing at its last value, and since
  the run loop is parked inside `connect()` for the whole window, nothing
  would publish another one. The waveform froze mid-shape over a session
  hearing nothing — which is the same lie `pump_audio`'s capture gate already
  refuses to tell for a muted mic, in its own words, "a muted mic that still
  animates reads as 'you are being heard'". `disconnect()` now zeroes the
  meter on its way out.

  This does not close the live-`session.update` follow-up, and is not meant
  to. A signalled deaf window is still a deaf window; a live swap would not
  have one. It stops the gap being silent about itself, which is what made it
  worth raising separately from the latency question.
- **Do the four required fields survive contact with real use?** The set
  is argued, not measured. The thing to watch is whether `manner` and
  `extra` collect the same content — if they do, the split is wrong.
  Write three real personas before building the editor.
- **Does an id need to be user-visible at all?** The model needs a
  stable handle, but the user arguably only ever needs the label.
  Proposed: derive the id from the label on creation, keep it fixed
  through renames, and never show it.
- ~~How many personas is too many?~~ **Settled: six.** Enforced at
  sidecar load, which caps the `switch_persona` enum at six short
  `use_when` lines — small enough not to degrade tool selection, and
  more personas than the situational use case needs.
