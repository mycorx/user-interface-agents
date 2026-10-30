// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The alert cards waiting in, or shown by, the `alert` window. The window
//! reads them on load (Tauri does not buffer events for late listeners) and
//! dismisses them one at a time by card id. Ids are opaque and never reused
//! within a run, so a dismiss can never hit a card that arrived after the
//! page last read the list.

use std::collections::VecDeque;

use uia_mcp::alert::Alert;

/// At most this many cards are kept; a newer one pushes the oldest out.
pub const MAX_CARDS: usize = 3;

#[derive(Debug, Default)]
pub struct CardQueue {
    cards: VecDeque<(u64, Alert)>,
    next_id: u64,
}

impl CardQueue {
    /// Add a card at the end, dropping the oldest beyond [`MAX_CARDS`].
    /// Returns the new card's id.
    pub fn push(&mut self, alert: Alert) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.cards.push_back((id, alert));
        while self.cards.len() > MAX_CARDS {
            self.cards.pop_front();
        }
        id
    }

    /// The cards with their ids, oldest first. Reading does not clear them.
    pub fn cards(&self) -> impl Iterator<Item = (u64, &Alert)> {
        self.cards.iter().map(|(id, alert)| (*id, alert))
    }

    /// Remove the card with `id`. False when there is none.
    pub fn dismiss(&mut self, id: u64) -> bool {
        match self.cards.iter().position(|(card, _)| *card == id) {
            Some(index) => self.cards.remove(index).is_some(),
            None => false,
        }
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
        queue.cards().map(|(_, a)| a.summary.as_str()).collect()
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
    fn dismissing_a_card_by_id_removes_only_that_card_even_after_a_new_one_arrives() {
        let mut queue = CardQueue::default();
        queue.push(alert("one"));
        let two = queue.push(alert("two"));
        queue.push(alert("three"));
        // The page read [one, two, three]; "four" pushes "one" out before
        // the dismiss for "two" lands. A position would now hit "three".
        queue.push(alert("four"));
        assert!(queue.dismiss(two));
        assert_eq!(summaries(&queue), ["three", "four"]);
    }

    #[test]
    fn dismissing_an_unknown_id_changes_nothing() {
        let mut queue = CardQueue::default();
        let one = queue.push(alert("one"));
        assert!(!queue.dismiss(one + 1));
        assert_eq!(summaries(&queue), ["one"]);
    }

    #[test]
    fn card_ids_are_never_reused_after_a_dismiss() {
        let mut queue = CardQueue::default();
        let one = queue.push(alert("one"));
        assert!(queue.dismiss(one));
        let two = queue.push(alert("two"));
        assert_ne!(one, two);
        assert!(!queue.dismiss(one));
        assert_eq!(summaries(&queue), ["two"]);
    }

    #[test]
    fn the_queue_is_empty_once_the_last_card_is_dismissed() {
        let mut queue = CardQueue::default();
        assert!(queue.is_empty());
        let one = queue.push(alert("one"));
        assert!(!queue.is_empty());
        assert!(queue.dismiss(one));
        assert!(queue.is_empty());
    }
}
