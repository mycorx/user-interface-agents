// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! macOS-native echo cancellation, via the Voice Processing IO audio unit.
//!
//! macOS's equivalent of `crate::wasapi`: audio played through a VPIO unit is
//! the echo reference, and audio recorded through the same unit has it
//! cancelled, plus Apple's AGC and noise suppression. No bundled C++, no
//! toolchain — unlike [`crate::aec`], which `uia-app` never composes with
//! this.
//!
//! One unit serves both directions, so unlike WASAPI's two independent
//! streams, `CoreAudioSource` and `CoreAudioSink` share one supervisor thread
//! that owns the unit, rebuilds it when a bound device disappears or (with no
//! device configured) the system default moves, and backs off patiently when
//! a device is gone. Spike results behind these choices are in
//! `docs/superpowers/specs/2026-10-07-macos-native-aec-design.md`.

mod capture;
mod device;
mod render;
mod unit;

pub use capture::CoreAudioSource;
pub use render::CoreAudioSink;

use crate::backoff::reopen_delay_ms;
use device::{DeviceInfo, Direction, Listeners};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;
use uia_core::audio::{AudioError, AudioFormat, Encoding};
use unit::{CLIENT_RATE_HZ, FramesTx, Shared, VoiceUnit};

/// Backstop re-check for notifications CoreAudio did not deliver.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// What wakes the supervisor thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// A CoreAudio property the supervisor listens to changed.
    Changed,
    /// The backend is being dropped.
    Stop,
}

/// The app-side format of both source and sink: fixed, see `CLIENT_RATE_HZ`.
/// Mono is safe to report for the sink because the session only reads
/// `sample_rate_hz` from either format.
fn app_format() -> AudioFormat {
    AudioFormat {
        sample_rate_hz: CLIENT_RATE_HZ,
        channels: 1,
        encoding: Encoding::Pcm16Le,
    }
}

/// Whether the unit must be rebuilt. `bound` is what the unit is using,
/// `alive` whether each still exists, `now` what the configuration resolves
/// to today (`None` when it resolves to nothing). Pinned names and followed
/// defaults both reduce to "does `now` still equal `bound`".
fn should_rebuild(bound: (u32, u32), alive: (bool, bool), now: Option<(u32, u32)>) -> bool {
    !alive.0 || !alive.1 || now != Some(bound)
}

/// Park for `delay` or until woken. False means stop.
fn wait_or_stop(rx: &Receiver<Wake>, delay: Duration) -> bool {
    match rx.recv_timeout(delay) {
        Ok(Wake::Stop) | Err(RecvTimeoutError::Disconnected) => false,
        Ok(Wake::Changed) | Err(RecvTimeoutError::Timeout) => true,
    }
}

/// The configured device names, owned for the supervisor thread.
struct Wanted {
    input: Option<String>,
    output: Option<String>,
}

impl Wanted {
    fn resolve(&self) -> Result<(DeviceInfo, DeviceInfo), AudioError> {
        Ok((
            device::resolve(Direction::Input, self.input.as_deref())?,
            device::resolve(Direction::Output, self.output.as_deref())?,
        ))
    }
}

/// What the supervisor reports once the unit is running.
struct Opened {
    input: String,
    output: String,
    processing_rate_hz: u32,
}

impl std::fmt::Display for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "input {:?} / output {:?}, ", self.input, self.output)?;
        if self.processing_rate_hz == 0 {
            write!(f, "processing rate unknown")?;
        } else {
            write!(f, "processing at {} Hz", self.processing_rate_hz)?;
        }
        if self.processing_rate_hz != 0 && self.processing_rate_hz < CLIENT_RATE_HZ {
            write!(
                f,
                " (the assistant's voice is band-limited on this device pairing)"
            )?;
        }
        Ok(())
    }
}

/// Owned jointly by the source and sink; dropping the last one stops the
/// supervisor, which drops the unit.
pub(crate) struct Backend {
    pub(crate) shared: Arc<Shared>,
    wake: Sender<Wake>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.wake.send(Wake::Stop);
        if let Some(worker) = self.worker.get_mut().ok().and_then(Option::take) {
            let _ = worker.join();
        }
    }
}

/// Open the echo-cancelling microphone and speaker as one Voice Processing
/// IO unit. `input`/`output`, when `Some`, pin the first device of that
/// direction whose name contains them (case-insensitive); `None` follows the
/// system default, including when it changes later.
///
/// Failure to open is returned here, never as a source that silently yields
/// nothing — `uia-app` falls back to plain cpal devices on any error.
pub fn open_voice_processing(
    input: Option<&str>,
    output: Option<&str>,
) -> Result<(CoreAudioSource, CoreAudioSink), AudioError> {
    // Bounded and small, as for `CpalSource`: live audio wants the newest
    // frame dropped, not a backlog.
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Shared::new());
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    // Leaked deliberately, one per backend: see `Listeners::register`.
    let listener_tx: &'static Sender<Wake> = Box::leak(Box::new(wake_tx.clone()));
    let wanted = Wanted {
        input: input.map(str::to_owned),
        output: output.map(str::to_owned),
    };
    let worker_shared = Arc::clone(&shared);

    let worker = std::thread::Builder::new()
        .name("uia-coreaudio-vpio".to_string())
        .spawn(move || {
            supervise(
                ready_tx,
                wake_rx,
                listener_tx,
                worker_shared,
                frames_tx,
                wanted,
            )
        })
        .map_err(|e| {
            AudioError::DeviceUnavailable(format!(
                "coreaudio: could not spawn the supervisor thread: {e}"
            ))
        })?;

    let opened = match ready_rx.recv() {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => {
            let _ = worker.join();
            return Err(e);
        }
        Err(_) => {
            let _ = worker.join();
            return Err(AudioError::DeviceUnavailable(
                "coreaudio: the supervisor thread stopped before reporting readiness".to_string(),
            ));
        }
    };
    eprintln!(
        "uia-audio: coreaudio voice processing opened {opened} (OS echo cancellation active)"
    );

    let backend = Arc::new(Backend {
        shared,
        wake: wake_tx,
        worker: Mutex::new(Some(worker)),
    });
    Ok((
        CoreAudioSource::new(frames_rx, Arc::clone(&backend)),
        CoreAudioSink::new(backend),
    ))
}

/// Own the unit for the backend's lifetime: open, watch, rebuild, back off.
///
/// `frames_tx` lives on this thread's stack (each unit's callbacks hold only
/// a clone), so however this function ends — return or panic — the capture
/// channel closes and the source sees end-of-stream instead of hanging on a
/// dead microphone.
fn supervise(
    ready_tx: std::sync::mpsc::Sender<Result<Opened, AudioError>>,
    wake: Receiver<Wake>,
    listener_tx: &'static Sender<Wake>,
    shared: Arc<Shared>,
    frames_tx: FramesTx,
    wanted: Wanted,
) {
    let mut ready_tx = Some(ready_tx);
    let mut attempt = 0u32;
    let mut loss_logged = false;

    loop {
        let opened = wanted.resolve().and_then(|(input, output)| {
            VoiceUnit::open(input.id, output.id, &shared, &frames_tx).map(|u| (u, input, output))
        });
        let (voice, input, output) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(e));
                    return;
                }
                if !loss_logged {
                    eprintln!(
                        "uia-audio: coreaudio cannot reopen voice processing ({e}); still trying"
                    );
                    loss_logged = true;
                    // Nothing drains the queue while no unit runs; without
                    // this the assistant would resume mid-backlog after the
                    // outage. A quick successful rebuild never gets here, so
                    // a default-device switch keeps speaking seamlessly.
                    shared
                        .queue
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clear();
                }
                if !wait_or_stop(
                    &wake,
                    Duration::from_millis(reopen_delay_ms(attempt).into()),
                ) {
                    return;
                }
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        let bound = (input.id, output.id);
        let listeners = Listeners::register(listener_tx, &[input.id, output.id]);
        let info = Opened {
            input: input.name,
            output: output.name,
            processing_rate_hz: voice.processing_rate_hz,
        };
        match ready_tx.take() {
            Some(tx) => {
                if tx.send(Ok(info)).is_err() {
                    return;
                }
            }
            None => eprintln!("uia-audio: coreaudio voice processing reopened {info}"),
        }
        attempt = 0;
        loss_logged = false;

        loop {
            if !wait_or_stop(&wake, WATCH_INTERVAL) {
                return;
            }
            // Coalesce a burst of notifications into one check.
            while let Ok(w) = wake.try_recv() {
                if w == Wake::Stop {
                    return;
                }
            }
            let now = wanted.resolve().ok().map(|(i, o)| (i.id, o.id));
            let alive = (device::is_alive(bound.0), device::is_alive(bound.1));
            if should_rebuild(bound, alive, now) {
                eprintln!(
                    "uia-audio: coreaudio audio devices changed; rebuilding voice processing"
                );
                break;
            }
        }
        drop(listeners);
        drop(voice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const BOUND: (u32, u32) = (90, 107);

    /// Review focus 1: notification bursts with nothing actually changed
    /// must not tear the unit down.
    #[test]
    fn unchanged_bound_devices_never_rebuild() {
        assert!(!should_rebuild(BOUND, (true, true), Some(BOUND)));
    }

    #[test]
    fn a_dead_device_rebuilds() {
        assert!(should_rebuild(BOUND, (false, true), Some(BOUND)));
        assert!(should_rebuild(BOUND, (true, false), Some(BOUND)));
    }

    /// An unpinned default moved, or a pinned name now resolves elsewhere.
    #[test]
    fn a_different_resolution_rebuilds() {
        assert!(should_rebuild(BOUND, (true, true), Some((90, 81))));
        assert!(should_rebuild(BOUND, (true, true), Some((76, 107))));
    }

    /// A configured device that no longer resolves goes through the reopen
    /// backoff, not a silent switch to something else.
    #[test]
    fn an_unresolvable_configuration_rebuilds() {
        assert!(should_rebuild(BOUND, (true, true), None));
    }

    /// Review focus 2: dropping the backend mid-backoff must not wait out a
    /// five-second delay.
    #[test]
    fn a_stop_during_backoff_is_not_waited_out() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Wake::Stop).unwrap();
        let started = Instant::now();
        assert!(!wait_or_stop(&rx, Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "took {:?}",
            started.elapsed()
        );
    }

    /// `wait_or_stop`'s contract: a `Changed` wake ends the wait early and
    /// keeps going. During backoff the listeners are already dropped, so such
    /// a wake can only be a stale one queued before the unit went down — the
    /// supervisor does not react to device changes mid-backoff.
    #[test]
    fn a_changed_wake_ends_the_wait_early_but_keeps_going() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Wake::Changed).unwrap();
        let started = Instant::now();
        assert!(wait_or_stop(&rx, Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn an_unknown_processing_rate_is_not_reported_as_zero_hz() {
        let opened = Opened {
            input: "Mic".to_string(),
            output: "Speakers".to_string(),
            processing_rate_hz: 0,
        };
        let text = opened.to_string();
        assert!(text.contains("processing rate unknown"), "got: {text}");
        assert!(!text.contains("0 Hz"), "got: {text}");
        assert!(!text.contains("band-limited"), "got: {text}");
    }

    #[test]
    fn a_known_rate_below_48k_notes_band_limiting() {
        let opened = Opened {
            input: "Webcam".to_string(),
            output: "Speakers".to_string(),
            processing_rate_hz: 16_000,
        };
        assert_eq!(
            opened.to_string(),
            "input \"Webcam\" / output \"Speakers\", processing at 16000 Hz \
             (the assistant's voice is band-limited on this device pairing)"
        );
    }

    /// The source must see end-of-stream when the supervisor thread ends,
    /// even while something (here, as in `Backend`) still holds `Shared`.
    /// Runs the real `supervise` with a device name nothing can match, so it
    /// ends deterministically with or without audio hardware.
    #[test]
    fn the_capture_channel_closes_when_the_supervisor_ends() {
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(4);
        let shared = Arc::new(Shared::new());
        let (_wake_tx, wake_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let listener_tx: &'static Sender<Wake> = Box::leak(Box::new(std::sync::mpsc::channel().0));
        let wanted = Wanted {
            input: Some("definitely-not-a-real-device-9f3a1c".to_string()),
            output: None,
        };
        let worker_shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            supervise(
                ready_tx,
                wake_rx,
                listener_tx,
                worker_shared,
                frames_tx,
                wanted,
            )
        })
        .join()
        .unwrap();
        assert!(matches!(ready_rx.recv(), Ok(Err(_))));
        assert!(
            frames_rx.blocking_recv().is_none(),
            "the source would wait forever on a dead microphone"
        );
        drop(shared);
    }

    #[test]
    fn the_backends_stay_sendable_across_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<CoreAudioSource>();
        assert_send::<CoreAudioSink>();
    }

    use uia_core::audio::{AudioSink, AudioSource};

    #[tokio::test]
    #[ignore = "requires real audio devices; run manually on a Mac"]
    async fn the_default_devices_open_and_yield_frames() {
        let (mut source, sink) = open_voice_processing(None, None).unwrap();
        assert_eq!(source.format().sample_rate_hz, 48_000);
        assert_eq!(
            source.format().channels,
            1,
            "must advertise the mono it yields"
        );
        assert_eq!(sink.format().sample_rate_hz, 48_000);
        let frame = tokio::time::timeout(Duration::from_secs(2), source.next_frame())
            .await
            .expect("no frame within 2 s")
            .expect("source ended");
        assert!(!frame.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires real audio devices; run manually on a Mac"]
    async fn clear_empties_the_queue_synchronously() {
        let (_source, mut sink) = open_voice_processing(None, None).unwrap();
        sink.write(&[1; 48_000]).await.unwrap();
        sink.clear();
        assert_eq!(sink.buffered_len(), 0);
    }

    /// A `uia.toml` typo must look different from a missing device.
    #[test]
    #[ignore = "requires real audio devices; run manually on a Mac"]
    fn an_unmatched_device_name_is_a_clear_error() {
        let Err(err) = open_voice_processing(Some("definitely-not-a-real-device-9f3a1c"), None)
        else {
            panic!("expected an error for a device name that cannot exist");
        };
        assert!(
            err.to_string().contains("no audio device matching"),
            "got: {err}"
        );
    }

    /// The spike's parity check made permanent. Set UIA_TEST_INPUT and
    /// UIA_TEST_OUTPUT to two different devices' names.
    #[test]
    #[ignore = "requires two named devices; run manually on a Mac"]
    fn a_split_named_pair_opens() {
        let input = std::env::var("UIA_TEST_INPUT").expect("set UIA_TEST_INPUT");
        let output = std::env::var("UIA_TEST_OUTPUT").expect("set UIA_TEST_OUTPUT");
        let opened = open_voice_processing(Some(&input), Some(&output));
        assert!(opened.is_ok(), "{:?}", opened.err().map(|e| e.to_string()));
    }

    /// Barge-in budget: what still plays after `clear()` is one callback
    /// period, which VPIO chooses — so measure it rather than assert on a
    /// requested size.
    #[test]
    #[ignore = "requires real audio devices; run manually on a Mac"]
    fn the_render_callback_period_fits_the_barge_in_budget() {
        let (_source, sink) = open_voice_processing(None, None).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        let shared = &sink.backend.shared;
        let calls = shared
            .render_calls
            .load(std::sync::atomic::Ordering::Relaxed);
        let frames = shared
            .render_frames
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(calls > 0, "render callback never ran");
        let period_ms = frames as f64 / calls as f64 / 48.0;
        assert!(
            period_ms <= 50.0,
            "callback period {period_ms:.1} ms exceeds the 50 ms budget"
        );
    }
}
