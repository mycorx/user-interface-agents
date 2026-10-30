// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The session state as the alert coordinator sees it: the current value and
//! every later change, with no gap and no replay between the two.

use std::sync::Mutex;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use uia_core::session::State;

/// The session's current state, and every change of it, for the alert
/// coordinator. `set` stores and sends under the lock, so a reader that takes
/// the current value under the same lock and discards what is queued sees each
/// later change exactly once, in order.
pub struct SessionStateFeed {
    current: Mutex<State>,
    changes: UnboundedSender<State>,
}

impl SessionStateFeed {
    pub fn new(initial: State) -> (Self, UnboundedReceiver<State>) {
        let (changes, receiver) = unbounded_channel();
        (
            Self {
                current: Mutex::new(initial),
                changes,
            },
            receiver,
        )
    }

    pub fn set(&self, state: State) {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        *current = state;
        let _ = self.changes.send(state);
    }

    /// The current state, with everything already queued in `changes`
    /// discarded because `current` reflects it.
    pub fn snapshot_and_drain(&self, changes: &mut UnboundedReceiver<State>) -> State {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        while changes.try_recv().is_ok() {}
        *current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_queued_before_the_snapshot_are_not_replayed() {
        let (feed, mut changes) = SessionStateFeed::new(State::Idle);
        feed.set(State::Listening);
        feed.set(State::Thinking);
        assert_eq!(feed.snapshot_and_drain(&mut changes), State::Thinking);
        assert!(changes.try_recv().is_err());
    }

    #[test]
    fn changes_after_the_snapshot_arrive_in_order() {
        let (feed, mut changes) = SessionStateFeed::new(State::Idle);
        assert_eq!(feed.snapshot_and_drain(&mut changes), State::Idle);
        feed.set(State::Listening);
        feed.set(State::Thinking);
        assert_eq!(changes.try_recv().unwrap(), State::Listening);
        assert_eq!(changes.try_recv().unwrap(), State::Thinking);
        assert!(changes.try_recv().is_err());
    }
}
