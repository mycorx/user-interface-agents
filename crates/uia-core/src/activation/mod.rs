// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use async_trait::async_trait;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationEvent {
    Show,
    Hide,
    Toggle,
    Interrupt,
}

/// Abstracts "the user wants to talk now". Desktop supplies a global hotkey and
/// tray; mobile (SP6) supplies a push-to-talk button and lifecycle events.
/// Keeps the core from ever learning which platform it is on.
#[async_trait]
pub trait Activation: Send {
    async fn next(&mut self) -> Option<ActivationEvent>;
}

pub struct ChannelActivation {
    rx: mpsc::Receiver<ActivationEvent>,
}

impl ChannelActivation {
    pub fn new() -> (Self, mpsc::Sender<ActivationEvent>) {
        let (tx, rx) = mpsc::channel(16);
        (Self { rx }, tx)
    }
}

#[async_trait]
impl Activation for ChannelActivation {
    async fn next(&mut self) -> Option<ActivationEvent> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn channel_activation_delivers_pushed_events() {
        let (mut act, tx) = ChannelActivation::new();
        tx.send(ActivationEvent::Show).await.unwrap();
        tx.send(ActivationEvent::Interrupt).await.unwrap();
        assert_eq!(act.next().await, Some(ActivationEvent::Show));
        assert_eq!(act.next().await, Some(ActivationEvent::Interrupt));
    }

    #[tokio::test]
    async fn channel_activation_ends_when_sender_drops() {
        let (mut act, tx) = ChannelActivation::new();
        drop(tx);
        assert_eq!(act.next().await, None);
    }
}
