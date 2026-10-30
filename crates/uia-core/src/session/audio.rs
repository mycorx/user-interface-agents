// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Capture in its own task.
//!
//! The run loop used to await `source.next_frame()` once per pass, so a
//! microphone that yielded nothing froze everything else the loop does: engine
//! draining, parking, rotation, the idle deadline. Reading the source on a
//! task of its own and handing frames over a bounded channel lets the loop
//! `select!` over capture alongside its other inputs instead.

use crate::audio::{AudioFormat, AudioSource, DetachedSource};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// How many captured frames may wait for the session before capture blocks.
/// About 80 ms at the ~20 ms frames every device here delivers: enough to
/// ride out a slow pass of the run loop, small enough that a backed-up queue
/// never hands the engine a noticeable burst of stale audio.
const CAPTURE_QUEUE_DEPTH: usize = 4;

/// One frame as the source yielded it, with the rate it was captured at.
///
/// The rate travels with the frame because capture reads ahead of the
/// session: by the time a frame is handled, `source.format()` may already
/// describe a later one from a device that reopened at a new rate.
pub(crate) struct CapturedFrame {
    pub(crate) samples: Vec<i16>,
    pub(crate) sample_rate_hz: u32,
}

pub(crate) struct Capture {
    state: CaptureState,
    /// The source's format when it was handed over, so `stop` can still
    /// return a source of the right shape if the task lost the real one.
    format: AudioFormat,
}

enum CaptureState {
    /// Not started yet: the task is spawned by the first `recv`, so building
    /// a `Session` never needs a runtime.
    Idle(Box<dyn AudioSource>),
    Running {
        frames: mpsc::Receiver<CapturedFrame>,
        stop: oneshot::Sender<()>,
        task: JoinHandle<Box<dyn AudioSource>>,
    },
}

impl Capture {
    pub(crate) fn new(source: Box<dyn AudioSource>) -> Self {
        Self {
            format: source.format(),
            state: CaptureState::Idle(source),
        }
    }

    /// The next captured frame, or `None` once the source is exhausted.
    ///
    /// Cancel-safe: the only await is a channel receive, so the run loop can
    /// race this in a `select!` and lose no frame when another arm wins.
    pub(crate) async fn recv(&mut self) -> Option<CapturedFrame> {
        if let CaptureState::Idle(_) = self.state {
            self.start();
        }
        match &mut self.state {
            CaptureState::Running { frames, .. } => frames.recv().await,
            CaptureState::Idle(_) => unreachable!("started above"),
        }
    }

    /// End the capture task and hand the source back, undropped, so the
    /// caller decides when its device closes. A `DetachedSource` if the task
    /// lost it (it panicked inside the source).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the audio-pair swap is its first caller")
    )]
    pub(crate) async fn stop(&mut self) -> Box<dyn AudioSource> {
        let detached = CaptureState::Idle(Box::new(DetachedSource::new(self.format)));
        match std::mem::replace(&mut self.state, detached) {
            CaptureState::Idle(source) => source,
            CaptureState::Running { stop, task, .. } => {
                // The task may already have ended on its own (the source ran
                // out), in which case nobody is listening; that is fine.
                let _ = stop.send(());
                match task.await {
                    Ok(source) => source,
                    Err(_) => Box::new(DetachedSource::new(self.format)),
                }
            }
        }
    }

    fn start(&mut self) {
        let detached = CaptureState::Idle(Box::new(DetachedSource::new(self.format)));
        let CaptureState::Idle(source) = std::mem::replace(&mut self.state, detached) else {
            return;
        };
        let (tx, frames) = mpsc::channel(CAPTURE_QUEUE_DEPTH);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(capture(source, tx, stopped));
        self.state = CaptureState::Running { frames, stop, task };
    }
}

/// The capture task: read, tag, forward, until stopped or exhausted. Both
/// awaits race the stop signal, so a source that never yields and a session
/// that is not reading both still stop promptly. A dropped `Capture` drops
/// the stop sender, which stops the task the same way.
async fn capture(
    mut source: Box<dyn AudioSource>,
    tx: mpsc::Sender<CapturedFrame>,
    mut stopped: oneshot::Receiver<()>,
) -> Box<dyn AudioSource> {
    loop {
        let samples = tokio::select! {
            biased;
            _ = &mut stopped => return source,
            frame = source.next_frame() => match frame {
                Some(samples) => samples,
                None => return source,
            },
        };
        // Read right after the frame, which is when the trait promises it
        // describes that frame.
        let frame = CapturedFrame {
            samples,
            sample_rate_hz: source.format().sample_rate_hz,
        };
        tokio::select! {
            biased;
            _ = &mut stopped => return source,
            sent = tx.send(frame) => if sent.is_err() {
                return source;
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::FixtureSource;
    use std::time::Duration;

    #[tokio::test]
    async fn frames_arrive_in_order_tagged_with_their_rate_then_none() {
        let mut capture = Capture::new(Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(48_000),
            vec![vec![1, 2], vec![3, 4]],
        )));

        let first = capture.recv().await.expect("first frame");
        assert_eq!((first.samples, first.sample_rate_hz), (vec![1, 2], 48_000));
        let second = capture.recv().await.expect("second frame");
        assert_eq!(second.samples, vec![3, 4]);
        assert!(capture.recv().await.is_none(), "an exhausted source ends");
    }

    #[tokio::test]
    async fn stopping_hands_back_the_source_with_its_unread_frames() {
        // More frames than the queue holds, so the task is parked on a full
        // queue when it is stopped. Whatever it had queued is discarded; the
        // frames it never read are still in the source it hands back.
        let frames: Vec<Vec<i16>> = (0..10).map(|n| vec![n]).collect();
        let mut capture = Capture::new(Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            frames,
        )));
        assert_eq!(capture.recv().await.expect("a frame").samples, vec![0]);

        let mut source = capture.stop().await;

        let mut rest = Vec::new();
        while let Some(frame) = source.next_frame().await {
            rest.push(frame[0]);
        }
        assert!(!rest.is_empty(), "the source came back drained");
        assert_eq!(
            rest.last(),
            Some(&9),
            "the source came back without its tail"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_a_source_that_never_yields_does_not_wait_on_it() {
        let mut capture = Capture::new(Box::new(DetachedSource::new(AudioFormat::mono_pcm16(
            16_000,
        ))));
        let waited = tokio::time::timeout(Duration::from_millis(50), capture.recv()).await;
        assert!(waited.is_err(), "a detached source yielded a frame");

        let source = tokio::time::timeout(Duration::from_secs(1), capture.stop())
            .await
            .expect("stop must not wait for a frame that never comes");
        assert_eq!(source.format(), AudioFormat::mono_pcm16(16_000));
    }

    #[tokio::test]
    async fn stopping_before_the_first_read_returns_the_source_untouched() {
        let mut capture = Capture::new(Box::new(FixtureSource::new(
            AudioFormat::mono_pcm16(16_000),
            vec![vec![7]],
        )));
        let mut source = capture.stop().await;
        assert_eq!(source.next_frame().await, Some(vec![7]));
    }
}
