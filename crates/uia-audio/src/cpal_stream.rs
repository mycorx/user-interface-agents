// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! One cpal stream kept alive across lost devices and default changes.
//!
//! A [`Leg`] owns a supervisor thread that opens the stream, and reopens it
//! with [`crate::backoff::reopen_delay_ms`] when cpal reports it lost, when an
//! unpinned direction's OS default moves on a host that does not follow it by
//! itself, or while a pinned device is missing. The stream is built and
//! dropped on that thread, so nothing here needs `cpal::Stream` to be `Send`.

use crate::backoff::reopen_delay_ms;
use crate::follow::{self, FirstOpen, Flow, OpenFailure};
use crate::live::{LiveFormat, PlaybackQueue};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uia_core::audio::{AudioError, WaitingCell, downmix_to_mono, f32_to_i16};

/// How often an unpinned direction checks whether the OS default moved, on a
/// host that does not reroute the stream itself. Well inside the two seconds
/// the spec allows for following a new default.
const DEFAULT_POLL: Duration = Duration::from_secs(1);

/// What a stream error asks of the supervisor.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamAction {
    /// The stream is still running.
    Ignore,
    /// The stream is gone: drop it and open another.
    Reopen,
}

/// Does this cpal stream error end the stream?
///
/// `DeviceChanged` is the host rerouting a default stream by itself, and
/// xruns and refused realtime promotion are glitches, not losses. Everything
/// else reopens, including kinds a later cpal adds: a needless reopen costs a
/// moment of audio, a missed one costs the device for good.
pub(crate) fn stream_error_action(kind: cpal::ErrorKind) -> StreamAction {
    match kind {
        cpal::ErrorKind::DeviceChanged
        | cpal::ErrorKind::Xrun
        | cpal::ErrorKind::RealtimeDenied => StreamAction::Ignore,
        _ => StreamAction::Reopen,
    }
}

/// When an unpinned direction next checks the OS default.
///
/// A deadline rather than a receive timeout: ignored stream errors (xruns,
/// reroutes) can arrive faster than [`DEFAULT_POLL`], and restarting a full
/// wait on each one would put the check off for as long as they keep coming.
struct PollClock {
    next: Instant,
}

impl PollClock {
    fn new(now: Instant) -> Self {
        Self {
            next: now + DEFAULT_POLL,
        }
    }

    /// How long to wait for a message before the check is due.
    fn wait(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }

    /// Is the check due at `now`? If so, the next one is a poll later.
    fn take_due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + DEFAULT_POLL;
        true
    }
}

/// What the supervisor thread is told.
enum Msg {
    /// A stream reported an error. `generation` names which stream, so a late
    /// error from one already replaced cannot tear down its successor.
    Error {
        generation: u64,
        kind: cpal::ErrorKind,
    },
    Stop,
}

/// Where a leg's audio goes or comes from.
enum Direction {
    Capture(tokio::sync::mpsc::Sender<Vec<i16>>),
    Render(PlaybackQueue),
}

impl Direction {
    fn flow(&self) -> Flow {
        match self {
            Direction::Capture(_) => Flow::Capture,
            Direction::Render(_) => Flow::Render,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Direction::Capture(_) => "input",
            Direction::Render(_) => "output",
        }
    }

    fn resolve(
        &self,
        host: &cpal::Host,
        pinned: Option<&str>,
    ) -> Result<cpal::Device, OpenFailure> {
        match (self, pinned) {
            (Direction::Capture(_), Some(name)) => {
                crate::input::find_input_device_by_name(host, name)
            }
            (Direction::Render(_), Some(name)) => {
                crate::output::find_output_device_by_name(host, name)
            }
            (Direction::Capture(_), None) => host.default_input_device().ok_or_else(|| {
                OpenFailure::Other(AudioError::DeviceUnavailable(
                    "no default input device".into(),
                ))
            }),
            (Direction::Render(_), None) => host.default_output_device().ok_or_else(|| {
                OpenFailure::Other(AudioError::DeviceUnavailable(
                    "no default output device".into(),
                ))
            }),
        }
    }

    fn default_id(&self, host: &cpal::Host) -> Option<cpal::DeviceId> {
        let device = match self {
            Direction::Capture(_) => host.default_input_device(),
            Direction::Render(_) => host.default_output_device(),
        };
        device.and_then(|d| d.id().ok())
    }
}

/// A playing stream and what it was opened on.
struct Bound {
    // Held only to keep the stream alive; cpal stops it on drop.
    _stream: cpal::Stream,
    id: Option<cpal::DeviceId>,
    rate: u32,
}

/// One direction's stream, kept open by its own supervisor thread.
pub(crate) struct Leg {
    tx: mpsc::Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Leg {
    /// Capture from `device_name`, or the default input, into `frames`.
    pub(crate) fn capture(
        device_name: Option<&str>,
        frames: tokio::sync::mpsc::Sender<Vec<i16>>,
        format: Arc<LiveFormat>,
        waiting: WaitingCell,
    ) -> Result<Leg, AudioError> {
        Self::start(Direction::Capture(frames), device_name, format, waiting)
    }

    /// Play `playback` on `device_name`, or the default output.
    pub(crate) fn render(
        device_name: Option<&str>,
        playback: PlaybackQueue,
        format: Arc<LiveFormat>,
        waiting: WaitingCell,
    ) -> Result<Leg, AudioError> {
        Self::start(Direction::Render(playback), device_name, format, waiting)
    }

    /// Returns once the first open has either succeeded, settled into waiting
    /// for a missing pinned device, or failed some other way. Never blocks
    /// for a device to appear: the session opens pairs under
    /// `spawn_blocking` and awaits them.
    fn start(
        direction: Direction,
        device_name: Option<&str>,
        format: Arc<LiveFormat>,
        waiting: WaitingCell,
    ) -> Result<Leg, AudioError> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let supervisor = Supervisor {
            pinned: device_name.map(str::to_string),
            direction,
            format,
            waiting,
            tx: tx.clone(),
            rx,
            generation: 0,
        };
        let thread = std::thread::Builder::new()
            .name(format!("uia-cpal-{}", supervisor.direction.label()))
            .spawn(move || supervisor.run(ready_tx))
            .map_err(|e| {
                AudioError::DeviceUnavailable(format!("cpal: no supervisor thread: {e}"))
            })?;
        let leg = Leg {
            tx,
            thread: Some(thread),
        };
        match ready_rx.recv() {
            Ok(result) => result.map(|()| leg),
            Err(_) => Err(AudioError::DeviceUnavailable(
                "cpal: the stream supervisor exited before opening".into(),
            )),
        }
    }
}

impl Drop for Leg {
    /// Stops the supervisor and joins it. A backoff is a wait on the same
    /// channel `Stop` arrives on, so this never sits out the rest of one.
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The state the supervisor thread owns.
struct Supervisor {
    direction: Direction,
    pinned: Option<String>,
    format: Arc<LiveFormat>,
    waiting: WaitingCell,
    /// Cloned into each stream's error callback.
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    /// Counts streams built, tagging their errors.
    generation: u64,
}

impl Supervisor {
    fn run(mut self, ready: mpsc::SyncSender<Result<(), AudioError>>) {
        let host = cpal::default_host();
        let follows = follow::host_follows_default(host.id().name(), self.direction.flow());
        let mut bound = match self.open(&host) {
            Ok(b) => {
                let _ = ready.send(Ok(()));
                Some(b)
            }
            Err(failure) => match follow::first_open(&failure) {
                FirstOpen::Wait(what) => {
                    eprintln!(
                        "uia-audio: {}: {}",
                        self.direction.label(),
                        AudioError::from(failure)
                    );
                    self.waiting.set(Some(what));
                    let _ = ready.send(Ok(()));
                    None
                }
                FirstOpen::Fail => {
                    let _ = ready.send(Err(failure.into()));
                    return;
                }
            },
        };
        drop(ready);
        // The rate of the stream the playback queue was last filled for.
        let mut last_rate = bound.as_ref().map_or(0, |b| b.rate);
        // Failed attempts since the last stream was lost; a first open that is
        // waiting has already made one.
        let mut attempt: u32 = u32::from(bound.is_none());
        let mut retry_now = false;
        let mut poll = PollClock::new(Instant::now());

        loop {
            if let Some(current) = &bound {
                let mut reopen = false;
                match self.rx.recv_timeout(poll.wait(Instant::now())) {
                    Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Ok(Msg::Error { generation, kind }) => {
                        reopen = generation == self.generation
                            && stream_error_action(kind) == StreamAction::Reopen;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
                // Whatever arrived: a stream of ignored errors must not keep
                // putting the check off.
                if !reopen && self.pinned.is_none() && !follows && poll.take_due(Instant::now()) {
                    let now = self.direction.default_id(&host);
                    reopen = now.is_some() && current.id.is_some() && now != current.id;
                }
                if reopen {
                    bound = None;
                    retry_now = true;
                }
                continue;
            }

            if !retry_now && !self.pause(reopen_delay_ms(attempt.saturating_sub(1))) {
                return;
            }
            retry_now = false;
            match self.open(&host) {
                Ok(b) => {
                    self.waiting.set(None);
                    if let Direction::Render(queue) = &self.direction
                        && !follow::keep_queue_on_reopen(last_rate, b.rate)
                    {
                        queue.clear();
                    }
                    last_rate = b.rate;
                    attempt = 0;
                    bound = Some(b);
                }
                Err(failure) => {
                    if attempt == 0 {
                        eprintln!(
                            "uia-audio: {}: reopen failed, retrying: {}",
                            self.direction.label(),
                            AudioError::from(failure)
                        );
                    }
                    self.waiting
                        .set(Some(follow::waiting_for(self.pinned.as_deref())));
                    if let Direction::Render(queue) = &self.direction {
                        queue.clear();
                    }
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    /// Waits `ms` unless told to stop first; `false` means stop. Errors that
    /// arrive meanwhile are from a stream already dropped.
    fn pause(&self, ms: u32) -> bool {
        let deadline = Instant::now() + Duration::from_millis(u64::from(ms));
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
                Ok(Msg::Error { .. }) => {}
                Err(RecvTimeoutError::Timeout) => return true,
            }
        }
    }

    /// Resolve the device and start a stream on it, publishing its format.
    fn open(&mut self, host: &cpal::Host) -> Result<Bound, OpenFailure> {
        let device = self.direction.resolve(host, self.pinned.as_deref())?;
        let other =
            |e: cpal::Error| OpenFailure::Other(AudioError::DeviceUnavailable(e.to_string()));
        let config = match self.direction {
            Direction::Capture(_) => device.default_input_config(),
            Direction::Render(_) => device.default_output_config(),
        }
        .map_err(other)?;
        let rate = config.sample_rate();
        let channels = config.channels();
        let stream_config: cpal::StreamConfig = config.into();

        self.generation += 1;
        let generation = self.generation;
        let errors = self.tx.clone();
        let label = self.direction.label();
        let on_error = move |err: cpal::Error| {
            eprintln!("uia-audio: {label} stream error: {err}");
            let _ = errors.send(Msg::Error {
                generation,
                kind: err.kind(),
            });
        };

        let stream = match &self.direction {
            Direction::Capture(frames) => {
                // Bounded upstream and small: a full channel means the
                // consumer has fallen behind, so the newest frame is dropped
                // rather than stalling the audio thread.
                let frames = frames.clone();
                device.build_input_stream(
                    stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        let pcm = f32_to_i16(data);
                        let mono = downmix_to_mono(&pcm, channels);
                        let _ = frames.try_send(mono);
                    },
                    on_error,
                    None,
                )
            }
            Direction::Render(queue) => {
                let queue = queue.clone();
                let slots = usize::from(channels.max(1));
                device.build_output_stream(
                    stream_config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        queue.try_fill(data, slots)
                    },
                    on_error,
                    None,
                )
            }
        }
        .map_err(other)?;
        stream.play().map_err(other)?;
        self.format.store(rate, channels);
        Ok(Bound {
            _stream: stream,
            id: device.id().ok(),
            rate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Errors that need no reopen arriving faster than the poll must not put
    /// the default check off: it stays due a poll after the last one.
    #[test]
    fn the_default_check_stays_due_while_ignored_errors_keep_arriving() {
        let start = Instant::now();
        let mut clock = PollClock::new(start);
        let ms = |n| start + Duration::from_millis(n);
        for at in [300, 600, 900] {
            assert!(!clock.take_due(ms(at)), "not due at {at} ms");
            assert_eq!(
                clock.wait(ms(at)),
                DEFAULT_POLL - Duration::from_millis(at),
                "the wait counts down to the deadline, at {at} ms"
            );
        }
        assert!(clock.take_due(ms(1_200)), "due once a poll has passed");
        assert_eq!(
            clock.wait(ms(1_200)),
            DEFAULT_POLL,
            "and the next is a poll later"
        );
        assert_eq!(
            clock.wait(ms(5_000)),
            Duration::ZERO,
            "an overdue check waits for nothing"
        );
    }

    #[test]
    fn device_changed_needs_no_reopen() {
        assert_eq!(
            stream_error_action(cpal::ErrorKind::DeviceChanged),
            StreamAction::Ignore
        );
    }

    #[test]
    fn xruns_and_realtime_denials_need_no_reopen() {
        for kind in [cpal::ErrorKind::Xrun, cpal::ErrorKind::RealtimeDenied] {
            assert_eq!(stream_error_action(kind), StreamAction::Ignore, "{kind:?}");
        }
    }

    #[test]
    fn a_lost_device_or_invalidated_stream_reopens() {
        for kind in [
            cpal::ErrorKind::DeviceNotAvailable,
            cpal::ErrorKind::StreamInvalidated,
            cpal::ErrorKind::BackendError,
            cpal::ErrorKind::Other,
        ] {
            assert_eq!(stream_error_action(kind), StreamAction::Reopen, "{kind:?}");
        }
    }
}
