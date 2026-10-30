# MCP alerts: manual test checklist

Windows, focus and hover cannot be unit-tested. Run this list by hand and
paste the results into the PR's "## Testing". Set up with the test server in
[MCP.md](MCP.md#trying-alerts-with-the-test-server); set
`ALERT_FEED_INTERVAL_SECS` to `5` to go faster, and restart UIA's connection
(disable and re-enable the server) to replay the sequence.

## Card

- [ ] Alert with an action: card shows title, summary, a Dismiss button and a
      "Join" button with `meet.google.com` on it.
- [ ] Alert without an action (the plain alert): no action button.
- [ ] Join opens the link in the browser and dismisses the card.
- [ ] Dismiss closes only that card.
- [ ] Clicking the card body brings the main window forward.
- [ ] First click works without the card having focus.
- [ ] A summary containing `<b>x</b>` (edit the example's text temporarily)
      shows the characters literally, not bold.
- [ ] Card is visible with the main window hidden.
- [ ] Two alerts at once show two stacked cards; four at once show 3 (the
      oldest is dropped).
- [ ] A card auto-dismisses after 30 s.
- [ ] Hovering a card pauses the 30 s timer (macOS non-focusable window:
      mouseenter/mouseleave were not verified in development).
- [ ] The card window is sized to its content: the empty area around the cards
      does not block clicks on whatever is behind it, and the window hides
      when the last card goes.
- [ ] Documented limit: on macOS the card does not appear over another app's
      full-screen Space.

## Feed handling

- [ ] The entry without a `summary` produces no card and no error.
- [ ] Reconnecting does not replay old entries (baseline); an entry created
      within the last 60 s before connecting does appear.
- [ ] The same `(id, stage)` read again does not produce a second card.

## Routing (agent wake-up on, Settings → MCP → "Let the agent speak alerts")

- [ ] Wake-up off, window hidden: card.
- [ ] Wake-up on, main window hidden: card.
- [ ] Wake-up on, main window minimised: card.
- [ ] Wake-up on, no session open, text turns supported: a session opens, the
      agent speaks the alert, the session closes after she finishes.
- [ ] Same, but you start talking before she finishes: the session is handed
      over and not closed.
- [ ] Wake-up on, session open and she is waiting: she speaks it now, no card.
- [ ] Wake-up on, session open and she is mid-reply (thinking, tool running,
      speaking): the alert is spoken after her current reply.
- [ ] Held alert not spoken within 30 s (keep her busy): a card appears.
- [ ] Engine without typed turns: card.
- [ ] Opening the session fails (for example no API key): card.
- [ ] A held T-5 is replaced when T-2 for the same meeting arrives.

## Settings

- [ ] "Show alerts" and "Let the agent speak alerts" on both a local and a
      remote server; both persist after closing and reopening Settings.
- [ ] Toggles apply without a restart (turn Show off: the next alert is
      dropped; on: cards resume).
- [ ] Show off disables and clears the second toggle.
- [ ] A failed save (for example remove the server's entry while the panel is
      open) shows the error and the checkbox visibly reverts.
- [ ] Row layout looks right at the normal and a narrow window width.

## Compatibility

- [ ] A server on an older protocol connects, its tools work, one info log
      line says it has no alerts, and no alerts arrive.
- [ ] One real legacy stdio server still connects and lists its tools (the
      handshake now tries `server/discover` first).
- [ ] One real legacy HTTP server still connects and lists its tools.
- [ ] `alert_resource` override: for a server whose feed lacks the MIME type,
      set `"alerts": {"alert_resource": "<uri>"}` in `uia-mcp.json` and
      confirm its alerts appear.

## End to end

- [ ] mymy-assistant, once its alerts resource ships, with
      `MYMY_PUSH_EVENTS` and `ENABLE_SYSTEM_TOASTS` off: the T-10/T-5/T-2/T-0
      ladder and an `invited` alert arrive as cards, and as speech with
      wake-up on; Join opens the Meet link.
