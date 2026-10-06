// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! `switch_persona`, layered over the MCP executor.
//!
//! Note that `clock_tool()` in `uia-engine-contract` is NOT a precedent for a
//! built-in tool — it is a test-harness fixture, dotted on purpose to
//! exercise `uia-mcp`'s wire-name sanitisation. In production every tool
//! reaching a session comes from `session::build_executor`, so this is the
//! first genuinely built-in tool, and it arrives as a decorator rather than
//! by modifying MCP dispatch.

use crate::personas::{
    PREVIOUS_ID, Persona, PersonaBook, SWITCH_PERSONA_TOOL, compose_prompt,
    switch_persona_descriptor,
};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use uia_core::session::{PromptSwapOutcome, SessionControl};
use uia_core::tools::{ToolDescriptor, ToolError, ToolExecutor, ToolResult};

/// A switch that has been answered but not yet landed: the `(current,
/// previous)` pair it *would* produce, under the tag the session will report
/// it by.
#[derive(Debug, Clone)]
struct PendingSwitch {
    tag: String,
    current: String,
    previous: Option<String>,
}

/// What persona this session is on, split by how sure we are of it.
///
/// The tool has to answer the model in the same breath as it asks -- the run
/// loop that actually rotates the connection is the very loop awaiting this
/// call, so waiting for the outcome here would deadlock it. So a switch is
/// recorded optimistically in `pending` and only becomes `settled` once the
/// session says it connected. A swap that failed to land discards `pending`
/// instead, leaving `settled` as what the session is really running.
#[derive(Debug)]
struct ActiveState {
    /// `(current, previous)` as of the last swap that actually landed.
    settled: (String, Option<String>),
    pending: Option<PendingSwitch>,
}

impl ActiveState {
    /// Where "previous" points: the optimistic answer while a switch is in
    /// flight — the model's own next turn must see the switch it just asked
    /// for — and the confirmed one otherwise.
    fn previous(&self) -> Option<&str> {
        match &self.pending {
            Some(pending) => pending.previous.as_deref(),
            None => self.settled.1.as_deref(),
        }
    }
}

pub struct PersonaExecutor {
    inner: Arc<dyn ToolExecutor>,
    book: PersonaBook,
    global_name: Option<String>,
    control: SessionControl,
    /// Held here rather than read back from the sidecar because a switch is
    /// answered before it has been applied, let alone persisted — and
    /// "previous" must mean the persona this *session* came from, not
    /// whatever a file last recorded.
    active: Mutex<ActiveState>,
    /// Outcomes of the swaps this decorator requested, drained lazily at the
    /// top of `execute`.
    ///
    /// Lazy rather than a spawned reconciler: the only thing that ever reads
    /// this state is the next tool call, so reacting sooner buys nothing,
    /// and it keeps the decorator free of a runtime at construction and the
    /// tests free of sleeps. Behind a `Mutex` because `execute` takes
    /// `&self`; nothing contends for it on any hot path.
    outcomes: Mutex<watch::Receiver<Option<PromptSwapOutcome>>>,
}

impl PersonaExecutor {
    pub fn new(
        inner: Arc<dyn ToolExecutor>,
        book: PersonaBook,
        global_name: Option<String>,
        control: SessionControl,
    ) -> Self {
        let active = book.active.clone();
        let outcomes = control.subscribe_prompt_swap_outcome();
        Self {
            inner,
            book,
            global_name,
            control,
            active: Mutex::new(ActiveState {
                settled: (active, None),
                pending: None,
            }),
            outcomes: Mutex::new(outcomes),
        }
    }

    /// Settle or discard the swap in flight, if the session has reported on
    /// it since the last call.
    ///
    /// Change detection, not a bare read: the watch holds its last value
    /// forever, so re-reading it would let an outcome about an earlier swap
    /// decide the fate of the one now pending — and when a persona is asked
    /// for twice the two share a tag, so matching on the tag alone would not
    /// notice.
    fn reconcile(&self) {
        let outcome = {
            let mut outcomes = self.outcomes.lock().expect("prompt swap outcome mutex");
            // `Err` means the session and every other clone are gone; there
            // will never be another outcome, so there is nothing to settle.
            if !outcomes.has_changed().unwrap_or(false) {
                return;
            }
            outcomes.borrow_and_update().clone()
        };
        let Some(outcome) = outcome else { return };

        let mut active = self.active.lock().expect("active persona mutex");
        if active.pending.as_ref().is_none_or(|p| p.tag != outcome.tag) {
            // About a swap that has already been superseded. `settled` is
            // still the last thing known to have landed either way.
            return;
        }
        let pending = active.pending.take().expect("checked just above");
        if outcome.applied {
            active.settled = (pending.current, pending.previous);
        }
    }

    /// Which persona an argument names, or the reason it names none. Kept
    /// separate from `execute` so every refusal is a value rather than an
    /// early return buried in the call path.
    fn resolve(&self, id: &str) -> Result<Persona, String> {
        if !self.book.allow_agent_switch {
            // Reachable: a model can call a tool it was never offered, and
            // "not declared" must not mean "unguarded".
            return Err("Persona switching is turned off in Settings.".to_string());
        }
        let wanted = if id == PREVIOUS_ID {
            self.active
                .lock()
                .expect("active persona mutex")
                .previous()
                .map(str::to_string)
                .ok_or_else(|| "There is no previous persona to go back to.".to_string())?
        } else {
            id.to_string()
        };

        self.book
            .personas
            .iter()
            .find(|p| p.id == wanted && p.enabled)
            .cloned()
            .ok_or_else(|| format!("There is no enabled persona called \"{wanted}\"."))
    }
}

#[async_trait]
impl ToolExecutor for PersonaExecutor {
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
        let mut tools = self.inner.list_tools().await?;
        // Prepended rather than appended: the descriptor list is what the
        // model reads top-down, and identity is not a footnote to whatever
        // MCP servers happen to be installed.
        if let Some(descriptor) = switch_persona_descriptor(&self.book) {
            tools.insert(0, descriptor);
        }
        Ok(tools)
    }

    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<ToolResult, ToolError> {
        if name != SWITCH_PERSONA_TOOL {
            return self.inner.execute(name, args, timeout).await;
        }

        // Before `resolve`, which reads `previous`: whatever the run loop
        // made of the last switch is what "previous" has to be measured
        // against.
        self.reconcile();

        let id = args.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let persona = match self.resolve(id) {
            Ok(persona) => persona,
            // A refusal is a normal tool result, not a `ToolError`: the model
            // should tell the user why and carry on, not see the transport
            // fail.
            Err(reason) => return Ok(ToolResult::error(reason)),
        };

        let prompt = compose_prompt(
            &persona,
            self.global_name.as_deref(),
            self.book.allow_agent_switch,
        );
        // Optimistic, and reversible. The model is told the switch happened
        // because it must be answered now, but the record of it stays in
        // `pending` until the session reports back through the outcome watch
        // — applied promotes it, failed drops it. Nothing here is ever the
        // last word on what the session is running.
        //
        // `previous` is taken from `settled`, not from the pending switch it
        // replaces: `request_prompt_swap` is latest-wins, so a switch
        // superseded before the loop took it is a place the session never
        // was, and going "back" to it would strand the user somewhere they
        // have not been.
        {
            let mut active = self.active.lock().expect("active persona mutex");
            let leaving = active.settled.0.clone();
            active.pending = Some(PendingSwitch {
                tag: persona.id.clone(),
                current: persona.id.clone(),
                previous: Some(leaving),
            });
        }
        // Returns immediately. The rotation happens on the run loop's next
        // pump: reconnecting from inside `handle_event`, which is what calls
        // this, would be re-entrant.
        self.control.request_prompt_swap(prompt, persona.id.clone());

        Ok(ToolResult::ok(format!(
            "Switched to {}. Tell the user you have switched.",
            persona.label
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uia_core::tools::FakeExecutor;

    fn book() -> PersonaBook {
        let mut book: PersonaBook =
            serde_json::from_str(include_str!("../tests/fixtures/personas.json")).unwrap();
        book.allow_agent_switch = true;
        book
    }

    fn executor() -> PersonaExecutor {
        PersonaExecutor::new(
            Arc::new(FakeExecutor::new().with_result("clock.now", ToolResult::ok("inner"))),
            book(),
            Some("MyMy".into()),
            SessionControl::new(),
        )
    }

    #[tokio::test]
    async fn switch_persona_is_declared_alongside_the_wrapped_tools() {
        let tools = executor().list_tools().await.unwrap();
        assert!(tools.iter().any(|t| t.name == SWITCH_PERSONA_TOOL));
        assert!(
            tools.iter().any(|t| t.name == "clock.now"),
            "inner tools survive"
        );
    }

    #[tokio::test]
    async fn every_other_tool_delegates_untouched() {
        let result = executor()
            .execute("clock.now", serde_json::json!({"city": "Sydney"}), TIMEOUT)
            .await
            .unwrap();
        assert_eq!(result.content, "inner");
    }

    #[tokio::test]
    async fn a_valid_switch_requests_a_prompt_swap() {
        let control = SessionControl::new();
        let exec = PersonaExecutor::new(
            Arc::new(FakeExecutor::new().with_result("clock.now", ToolResult::ok("inner"))),
            book(),
            Some("MyMy".into()),
            control.clone(),
        );

        let result = exec
            .execute(
                SWITCH_PERSONA_TOOL,
                serde_json::json!({"id": "secretary"}),
                TIMEOUT,
            )
            .await
            .unwrap();

        assert!(!result.is_error, "{result:?}");
        let swap = control.take_prompt_swap().expect("a swap was requested");
        assert_eq!(swap.tag, "secretary");
        assert!(swap.prompt.contains("executive secretary"));
    }

    #[tokio::test]
    async fn an_unknown_or_disabled_id_errors_and_swaps_nothing() {
        for id in ["nonexistent", "focus"] {
            let control = SessionControl::new();
            let exec = PersonaExecutor::new(
                Arc::new(FakeExecutor::new().with_result("clock.now", ToolResult::ok("inner"))),
                book(),
                None,
                control.clone(),
            );
            let result = exec
                .execute(SWITCH_PERSONA_TOOL, serde_json::json!({"id": id}), TIMEOUT)
                .await
                .unwrap();
            assert!(result.is_error, "{id} must be refused");
            assert!(control.take_prompt_swap().is_none(), "{id} must not rotate");
        }
    }

    #[tokio::test]
    async fn previous_returns_to_the_persona_in_use_before_the_last_switch() {
        let control = SessionControl::new();
        let exec = PersonaExecutor::new(
            Arc::new(FakeExecutor::new().with_result("clock.now", ToolResult::ok("inner"))),
            book(),
            None,
            control.clone(),
        );

        // No switch has happened, so there is nothing to go back to.
        let result = exec
            .execute(
                SWITCH_PERSONA_TOOL,
                serde_json::json!({"id": PREVIOUS_ID}),
                TIMEOUT,
            )
            .await
            .unwrap();
        assert!(result.is_error, "no prior persona yet");

        exec.execute(
            SWITCH_PERSONA_TOOL,
            serde_json::json!({"id": "secretary"}),
            TIMEOUT,
        )
        .await
        .unwrap();
        control.take_prompt_swap();

        exec.execute(
            SWITCH_PERSONA_TOOL,
            serde_json::json!({"id": PREVIOUS_ID}),
            TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(control.take_prompt_swap().unwrap().tag, "default");
    }

    #[tokio::test]
    async fn with_the_toggle_off_the_tool_is_neither_declared_nor_executable() {
        let mut off = book();
        off.allow_agent_switch = false;
        let control = SessionControl::new();
        let exec = PersonaExecutor::new(
            Arc::new(FakeExecutor::new().with_result("clock.now", ToolResult::ok("inner"))),
            off,
            None,
            control.clone(),
        );

        let tools = exec.list_tools().await.unwrap();
        assert!(!tools.iter().any(|t| t.name == SWITCH_PERSONA_TOOL));

        // Declared or not, a model that invents the call must not be obeyed.
        let result = exec
            .execute(
                SWITCH_PERSONA_TOOL,
                serde_json::json!({"id": "secretary"}),
                TIMEOUT,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(control.take_prompt_swap().is_none());
    }

    /// Three enabled personas, so a switch can be made *from* somewhere the
    /// session really was to somewhere it never got to.
    fn three_persona_book() -> PersonaBook {
        let mut book = book();
        book.personas
            .iter_mut()
            .find(|p| p.id == "focus")
            .expect("the fixture has a focus persona")
            .enabled = true;
        book
    }

    fn executor_on(control: SessionControl, book: PersonaBook) -> PersonaExecutor {
        PersonaExecutor::new(
            Arc::new(FakeExecutor::new().with_result("clock.now", ToolResult::ok("inner"))),
            book,
            None,
            control,
        )
    }

    /// Drive one switch to completion: request it, let the "run loop" take
    /// it, and report what became of it.
    async fn switch(exec: &PersonaExecutor, control: &SessionControl, id: &str) {
        exec.execute(
            SWITCH_PERSONA_TOOL,
            serde_json::json!({ "id": id }),
            TIMEOUT,
        )
        .await
        .unwrap();
        control.take_prompt_swap().expect("a swap was requested");
    }

    async fn previous_target(exec: &PersonaExecutor, control: &SessionControl) -> Option<String> {
        let result = exec
            .execute(
                SWITCH_PERSONA_TOOL,
                serde_json::json!({"id": PREVIOUS_ID}),
                TIMEOUT,
            )
            .await
            .unwrap();
        if result.is_error {
            return None;
        }
        Some(control.take_prompt_swap().expect("a swap").tag)
    }

    /// The bug this file used to carry. The decorator answered the model
    /// optimistically; when the post-swap connect failed the session was put
    /// back on the old prompt and nothing told the decorator, so `"previous"`
    /// afterwards named a persona the session had never left.
    #[tokio::test]
    async fn a_failed_swap_rolls_back_and_previous_returns_to_what_is_really_running() {
        let control = SessionControl::new();
        let exec = executor_on(control.clone(), three_persona_book());

        switch(&exec, &control, "secretary").await;
        control.note_prompt_swap_applied("secretary".into());

        // The session is on `secretary`. This one never lands.
        switch(&exec, &control, "focus").await;
        control.note_prompt_swap_failed("focus".into());

        assert_eq!(
            previous_target(&exec, &control).await.as_deref(),
            Some("default"),
            "the session is running secretary, so back is default -- not the \
             secretary it never left, and not a rollback the decorator missed"
        );
    }

    /// Two switches that both land. The second one's "previous" can only be
    /// `secretary` if the first was *promoted* into settled state -- a
    /// decorator that only ever discarded on failure and never promoted on
    /// success would answer `default` here, sending the user back past a
    /// persona they had really been using.
    #[tokio::test]
    async fn an_applied_swap_promotes_and_previous_returns_to_the_persona_it_left() {
        let control = SessionControl::new();
        let exec = executor_on(control.clone(), three_persona_book());

        switch(&exec, &control, "secretary").await;
        control.note_prompt_swap_applied("secretary".into());

        switch(&exec, &control, "focus").await;
        control.note_prompt_swap_applied("focus".into());

        assert_eq!(
            previous_target(&exec, &control).await.as_deref(),
            Some("secretary")
        );
        // And that answer is itself a switch, applied in turn: back again is
        // the persona it just left.
        control.note_prompt_swap_applied("secretary".into());
        assert_eq!(
            previous_target(&exec, &control).await.as_deref(),
            Some("focus")
        );
    }

    /// Latest-wins, as `request_prompt_swap` itself has it: the first swap is
    /// replaced before the loop ever applies it, so the session goes from the
    /// last settled persona straight to the second one -- and `"previous"`
    /// must name that settled persona, never the one that was skipped over.
    #[tokio::test]
    async fn a_second_switch_while_one_is_pending_replaces_it_and_settled_stays_put() {
        let control = SessionControl::new();
        let exec = executor_on(control.clone(), three_persona_book());

        exec.execute(
            SWITCH_PERSONA_TOOL,
            serde_json::json!({"id": "secretary"}),
            TIMEOUT,
        )
        .await
        .unwrap();
        switch(&exec, &control, "focus").await;
        control.note_prompt_swap_applied("focus".into());

        assert_eq!(
            previous_target(&exec, &control).await.as_deref(),
            Some("default"),
            "secretary was never applied, so it is not a place to go back to"
        );
    }

    /// The same shape, failing: with both pending swaps discarded the
    /// decorator is back on the persona it started on, with nothing behind
    /// it.
    #[tokio::test]
    async fn a_failed_second_switch_leaves_the_decorator_on_the_settled_persona() {
        let control = SessionControl::new();
        let exec = executor_on(control.clone(), three_persona_book());

        exec.execute(
            SWITCH_PERSONA_TOOL,
            serde_json::json!({"id": "secretary"}),
            TIMEOUT,
        )
        .await
        .unwrap();
        switch(&exec, &control, "focus").await;
        control.note_prompt_swap_failed("focus".into());

        assert_eq!(
            previous_target(&exec, &control).await,
            None,
            "nothing ever landed, so there is nowhere to go back to"
        );
    }

    /// Reconciliation must consume each outcome once. The watch keeps its
    /// last value forever, so a decorator that simply re-read it would apply
    /// an answer about an earlier swap to the swap now in flight -- and when
    /// the two share a tag, tag-matching alone does not catch it.
    #[tokio::test]
    async fn an_outcome_already_reconciled_is_not_applied_to_the_next_swap() {
        let control = SessionControl::new();
        let exec = executor_on(control.clone(), book());

        switch(&exec, &control, "secretary").await;
        control.note_prompt_swap_failed("secretary".into());
        // Reconciles the failure: back to `default`, nothing behind it.
        assert_eq!(previous_target(&exec, &control).await, None);

        // Asked again. The stale `secretary` failure must not discard it.
        switch(&exec, &control, "secretary").await;
        assert_eq!(
            previous_target(&exec, &control).await.as_deref(),
            Some("default"),
            "the pending switch is live, not retro-failed by an old outcome"
        );
    }

    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
}
