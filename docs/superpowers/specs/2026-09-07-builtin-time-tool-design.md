# Built-in tools, starting with the current time

## Problem

The assistant cannot say what time it is. Asked "what time is it" or
"what's today's date" it either refuses or invents an answer, and the
same happens for "what time is it in London".

This is not a knowledge gap, and that distinction drives the whole
design. A model cannot be taught the answer, because the answer is not
knowledge — it is *device state*. None of the obvious fixes reach it:

- **A bigger or different model** (routing the question to Haiku, GPT,
  or any second LLM) buys nothing. The realtime model is already an
  LLM; a second one has no clock either, and the extra round trip is
  audible in a voice loop.
- **A web search** has no page that knows the user's local time, and
  costs a network hop to fail.
- **A remote HTTP MCP server** answers with *its* clock in *its*
  timezone — wrong by construction, and confidently so.
- **A local `.mcpb` bundle** has the right clock, but is desktop-only
  (`McpServerConfig::is_desktop_only`, `crates/uia-mcp/src/client.rs`)
  so mobile builds get nothing, and every registry entry "starts
  disabled until you enable it" (`docs/MCP.md`), so a fresh install
  could not tell you the date until the user found Settings → MCP and
  installed a bundle.

The concept of a built-in tool does now exist. PR #85 added
`switch_persona` and, with it, the pattern: `PersonaExecutor`
(`crates/uia-app/src/persona_executor.rs`) is a **decorator** that
layers one non-MCP tool over whatever executor it wraps, joined to the
session by `session::persona_tools`. Its module doc is explicit that
this "arrives as a decorator rather than by modifying MCP dispatch."
So this spec adds the *second* built-in tool and follows that pattern
rather than inventing one.

One trap worth naming, because the name collides. `clock_tool()` in
`crates/uia-engine-contract/src/lib.rs` declares a tool called
`clock.now` that answers "what time is it in a city". It is **not** a
precedent and must not be reused or extended: it is a test-harness
fixture, deliberately dotted to exercise `uia-mcp`'s wire-name
sanitisation, backed by no implementation, and reachable only from the
live engine-contract tests. `persona_executor.rs` says so in its own
first paragraph. The real tool is unrelated to it and shares no code.

## Non-goals

- **Weather.** It is a separate, standalone MCP server published under
  Apache 2.0 in `mycorx/ai-agent-assets`, deliberately not shipped with
  UIA and not part of this workspace. It has its own spec.
- **Timers, alarms, reminders, scheduling.** Nothing here fires later;
  the tool answers a question and returns.
- **Date arithmetic** ("how many days until Christmas", "what's next
  Tuesday"). Given the current date and weekday, the model does this
  perfectly well on its own. Adding a tool for it means a second
  routing decision the model can get wrong, for no capability gain.
- **Calendar or timezone *conversion* between two arbitrary zones.**
  One zone per call. If the model wants two, it can ask twice, and in
  practice it converts correctly once it knows "now".
- **Per-tool confirmation.** `requires_confirmation` stays `false`,
  unchanged. Reading a clock mutates nothing.
- **A general built-in tool *framework*.** One executor with one tool.
  The second built-in tool is when a pattern is worth extracting, not
  before.

## The tool

One tool, `get_current_time`, taking one optional argument:

```json
{
  "type": "object",
  "properties": {
    "timezone": {
      "type": "string",
      "description": "IANA timezone identifier, e.g. \"Europe/London\" or \"Australia/Melbourne\". Omit for the device's own timezone."
    }
  }
}
```

**One tool rather than three.** `get_current_date` and
`get_day_of_week` would be separate routing decisions for the voice
model to get wrong, and every extra tool dilutes the description space
the model uses to choose. Date, weekday and time all ride in one
payload because they are all one clock reading.

**The result is a single short line of prose, not JSON.** The realtime
model has to *speak* this result, and a JSON object invites it to read
field names aloud or ramble through the structure. `ToolResult.content`
is a `String`; this fills it with something already close to an answer —
a single line, no newlines, wrapped here only to fit the page:

```
2026-09-07T23:10:04+10:00 — Sunday, 7 September 2026, 11:10 pm
(Australia/Melbourne, UTC+10:00, AEST)
```

The ISO-8601 stamp leads so the model has an unambiguous machine form
for any arithmetic it wants to do; the human phrasing follows so it has
something to say. The zone, offset and abbreviation are named because
"11:10 pm" alone is unverifiable — if the zone resolution went wrong,
the user should be able to hear that it went wrong.

## Timezones

Full IANA support, not just the local zone.

`chrono-tz` (0.10.4) compiles the IANA database into the binary. That
matters more than convenience: there is no network call, so the tool
works offline, on a plane, and on mobile, and it cannot fail for a
reason the user has to debug. `iana-time-zone` (0.1.65) resolves the
device's own zone when `timezone` is omitted. Both sit on `chrono`
(0.4.45), already in the tree.

**An unknown zone is a `ToolResult::error`, not a `ToolError`.** This
distinction is load-bearing. `ToolError` means the tool could not be
run — transport died, timed out, not found — and `Session` turns it
into a spoken failure. A bad argument is not that: the tool ran fine
and the *model* passed something wrong. Returning it as an error-flagged
result hands the model the information to correct itself, and the
message is written for that reader:

```
unknown timezone "PST" — expected an IANA identifier of the form
Area/Location, such as America/Los_Angeles
```

**No alias table, deliberately.** The temptation is to map `PST`,
`GMT`, `London` onto IANA names. Two reasons not to, yet: the tool
description already tells the model the exact form it wants, and models
follow that well; and every alias is a guess that can be wrong in a way
the user cannot see — `PST` is ambiguous about daylight saving in a way
`America/Los_Angeles` is not. If testing shows the model actually
fumbling the format, a small documented alias set is a cheap follow-up.
Starting strict keeps the failure visible instead of silently
mislocating someone.

## Dependencies, and the NOTICE step that goes with them

Less new than it looks. `chrono` 0.4.45 is already a direct dependency,
and `iana-time-zone` 0.1.65 is **already in `Cargo.lock`**, pulled in
transitively by chrono — promoting it to a direct dependency of
`uia-core` adds no new attribution, only an explicit line. So
**`chrono-tz` 0.10.4 is the only genuinely new crate**, plus whatever
its build script brings with it.

That still means a dependency change, and PR #85 established that a
dependency change in this repo has a step attached
(`scripts/gen-notice.sh`, and the header block `6b74d02` added to it):

- **Regenerate `NOTICE.txt` on Linux.** The cargo half comes from
  `cargo metadata` without `--filter-platform` and is
  platform-independent; the npm half comes from `pnpm licenses list`,
  which reads the *installed* store. Regenerating on Windows silently
  drops the four `*-linux-x64*` npm packages.
- **`--check` will not catch that.** It fails only on what is missing
  and treats locally-absent entries as "expected — other platforms'
  binaries". #85 hit exactly this: the cargo half went 785 → 791 while
  the npm half quietly went 60 → 56, and it passed `--check` on the
  machine that broke it. Additions are the point; removals are the bug.
  Diff `NOTICE.txt` and restore anything that vanished.
- The merged baseline to diff against is **791 crates and 60 npm
  packages**. This work should move the first number and leave the
  second alone.

One thing to **measure rather than assume**: `chrono-tz` compiles the
IANA database into the binary, which is the property that makes the
tool work offline, but it is not free. Its cost against Stage 8's
binary-size and idle-memory budget should be recorded from a real
build, not estimated here. Note that shrinking it by filtering zones is
not on the table — full IANA coverage is the requirement, not a
nice-to-have.

## Testability: the clock is injected

`TimeExecutor` takes its time source and its local-zone source as
constructor dependencies rather than calling `Utc::now()` and
`iana_time_zone::get_timezone()` inline. Without that the tool is
untestable by construction — every assertion would be against a value
that changed while the test ran, and the DST cases below could only be
tested twice a year.

```rust
pub trait Clock: Send + Sync {
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc>;
}

pub trait LocalZone: Send + Sync {
    fn local(&self) -> chrono_tz::Tz;
}
```

`SystemClock` and `SystemLocalZone` are the production implementations
and are the only code in the crate that touches the real clock.
`FixedClock` and `FixedZone` are their test counterparts, sitting
beside `FakeExecutor` in `crates/uia-core/src/tools/`.

## Wiring: a decorator, matching `PersonaExecutor`

`TimeExecutor` wraps a `ToolExecutor`, adds `get_current_time` to
`list_tools`, and passes every other name straight through to `inner` —
the same three moves `PersonaExecutor` makes. `uia-app` gains a
`session::time_tools` seam beside the existing `session::persona_tools`,
and the stack is built inside out:

```rust
let tools = persona_tools(time_tools(mcp_router), &book, global_name, control);
```

Layer order decides only the order tools are declared in.
`PersonaExecutor` prepends its descriptor because "identity is not a
footnote to whatever MCP servers happen to be installed"; with persona
outermost the model reads `switch_persona`, then `get_current_time`,
then the MCP tools — general capability ahead of installed capability.

**The tool name is bare: `get_current_time`.** That matches
`switch_persona`, and it needs no namespace to stay unique: every MCP
tool is `server.tool` and therefore contains a dot, so a dotless name
cannot collide with one. `PersonaExecutor::execute` already depends on
exactly that property. It also means `wire_tool_name` has nothing to
rewrite — the name already satisfies the `^[a-zA-Z0-9_-]+$` both OpenAI
and Nova enforce — so there is no `ToolNames` round trip and no
`validate_server_name` reservation to make.

### Why not an `McpRouter` entry

An earlier draft of this spec registered built-ins as one more
`("builtin", …)` pair in `McpRouter`. That is the wrong shape. It puts
a non-MCP tool inside MCP dispatch — the thing PR #85 deliberately
chose against — and it buys nothing, because the router's fan-out
exists to pick *between servers* by namespace and a built-in has no
server. Keeping it out also keeps `uia-mcp` free of any knowledge that
built-ins exist at all.

### One deliberate difference from `PersonaExecutor`

`PersonaExecutor::list_tools` propagates a wrapped failure with `?`, so
a failing inner executor takes `switch_persona` down with it.
`TimeExecutor` instead declares `get_current_time` even then. The clock
is the one tool that must not depend on an MCP server being reachable —
that independence is half the reason it is built in rather than shipped
as a bundle.

In practice the two never diverge, because `McpRouter::list_tools`
already swallows per-server failures and always returns `Ok`; the
difference is visible only when something else is wrapped. It is
recorded here so the inconsistency reads as a decision rather than an
oversight, and so that whoever eventually extracts a shared built-ins
executor knows this behaviour is the one to keep.

### Why a second decorator, not one shared built-ins executor

Two decorator layers is one more hop than one, and still right at this
size. `switch_persona` is stateful in a way a clock is not — it holds
`SessionControl`, an optimistic pending/settled split, and a rollback
path — so folding both into one executor would put a tool that mutates
the session next to one that reads a clock, and the shared type would
have to carry the union of their dependencies. A general built-in
*framework* stays a non-goal. If a third built-in arrives, that is the
moment to extract one.

## Testing

TDD: each case below is written as a failing test before the code that
satisfies it.

**The tool**

- A fixed clock and an explicit zone produce the exact expected string.
- `timezone` omitted uses the injected local zone.
- A half-hour offset (`Asia/Kolkata`, +05:30) and a 45-minute offset
  (`Asia/Kathmandu`, +05:45) format correctly — whole-hour-only offset
  formatting is a classic bug and both providers' users live in these
  zones.
- Either side of a DST transition in `Australia/Melbourne` returns the
  correct offset and abbreviation, and a northern-hemisphere zone
  (`Europe/London`) is checked too so a hemisphere assumption cannot
  hide.
- An unknown zone returns `is_error: true`, with the offending input
  quoted in the message, and does **not** return `Err`.
- An unexpected argument type (`timezone: 42`) is an error result, not
  a panic.

**Wiring**

- `TimeExecutor` wrapping a `FakeExecutor` lists the fake's tools *and*
  `get_current_time`.
- Any other tool name passes through to the wrapped executor untouched,
  arguments and all.
- A wrapped executor whose `list_tools` fails does not suppress
  `get_current_time` — the built-in is the one tool that cannot depend
  on an MCP server being reachable, which is half the reason it is
  built in.
- Stacked as `persona_tools(time_tools(fake))`, both built-ins are
  declared, `switch_persona` first, and neither shadows the other.

No test reads the real clock or the real system timezone.

## What this does not settle

The `location` question for the weather server — device geolocation,
then a user-set home location, then the server's own default — belongs
to that server's spec. Note only that `tauri-plugin-geolocation` is
Android/iOS only today (desktop is stubbed upstream), so on desktop the
first tier is dormant and the middle tier does the work.
