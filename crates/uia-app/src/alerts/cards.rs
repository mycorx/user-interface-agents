// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The alert cards waiting in, or shown by, the `alert` window. The window
//! reads them on load (Tauri does not buffer events for late listeners) and
//! dismisses them one at a time by position.

use std::collections::VecDeque;

use uia_mcp::alert::Alert;

/// At most this many cards are kept; a newer one pushes the oldest out.
pub const MAX_CARDS: usize = 3;

#[derive(Debug, Default)]
pub struct CardQueue {
    cards: VecDeque<Alert>,
}

impl CardQueue {
    /// Add a card at the end, dropping the oldest beyond [`MAX_CARDS`].
    pub fn push(&mut self, alert: Alert) {
        self.cards.push_back(alert);
        while self.cards.len() > MAX_CARDS {
            self.cards.pop_front();
        }
    }

    /// The cards, oldest first. Reading does not clear them.
    pub fn cards(&self) -> impl Iterator<Item = &Alert> {
        self.cards.iter()
    }

    /// Remove the card at `index` (oldest is 0). False when there is none.
    pub fn dismiss(&mut self, index: usize) -> bool {
        self.cards.remove(index).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(summary: &str) -> Alert {
        Alert {
            server: "calendar".into(),
            title: None,
            summary: summary.into(),
            details: None,
            kind: None,
            id: None,
            stage: None,
            created_at: None,
            action: None,
        }
    }

    fn summaries(queue: &CardQueue) -> Vec<&str> {
        queue.cards().map(|a| a.summary.as_str()).collect()
    }

    #[test]
    fn cards_are_kept_oldest_first_and_reading_them_does_not_clear_them() {
        let mut queue = CardQueue::default();
        queue.push(alert("one"));
        queue.push(alert("two"));
        assert_eq!(summaries(&queue), ["one", "two"]);
        assert_eq!(summaries(&queue), ["one", "two"]);
    }

    #[test]
    fn a_fourth_card_pushes_the_oldest_out() {
        let mut queue = CardQueue::default();
        for summary in ["one", "two", "three", "four"] {
            queue.push(alert(summary));
        }
        assert_eq!(summaries(&queue), ["two", "three", "four"]);
    }

    #[test]
    fn dismissing_a_card_removes_only_that_one() {
        let mut queue = CardQueue::default();
        for summary in ["one", "two", "three"] {
            queue.push(alert(summary));
        }
        assert!(queue.dismiss(1));
        assert_eq!(summaries(&queue), ["one", "three"]);
    }

    #[test]
    fn dismissing_a_card_that_is_not_there_changes_nothing() {
        let mut queue = CardQueue::default();
        queue.push(alert("one"));
        assert!(!queue.dismiss(1));
        assert_eq!(summaries(&queue), ["one"]);
    }

    #[test]
    fn the_queue_is_empty_once_the_last_card_is_dismissed() {
        let mut queue = CardQueue::default();
        assert!(queue.is_empty());
        queue.push(alert("one"));
        assert!(!queue.is_empty());
        assert!(queue.dismiss(0));
        assert!(queue.is_empty());
    }
}
