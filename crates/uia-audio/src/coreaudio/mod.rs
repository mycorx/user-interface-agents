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
//! a device is gone. A pinned name is resolved only when (re)opening, never
//! while a unit runs: a running unit makes every output device in this
//! process show an input stream (its echo-reference taps) and adds a private
//! aggregate device, so names are only resolved once a torn-down unit's
//! aggregate has left the list.
//!
//! VPIO processes both directions at the slower device's rate, so before each
//! open a bound device below 24 kHz that supports more is raised (to at most
//! 48 kHz) and restored once the unit releases it — see `rate`. A device that
//! cannot go higher keeps the assistant's voice band-limited, which the
//! opened line notes. Spike results behind these choices are in
//! `docs/superpowers/specs/2026-10-07-macos-native-aec-design.md`.

mod capture;
mod device;
mod rate;
mod render;
mod unit;

pub use capture::CoreAudioSource;
pub use render::CoreAudioSink;

use crate::backoff::reopen_delay_ms;
use device::{DeviceInfo, Direction, Listeners};
use rate::{FULL_VOICE_RATE_HZ, RateGuard};
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

/// How long to wait for a torn-down unit's private aggregate (and with it the
/// echo-reference taps) to leave the device list before resolving names.
const TEARDOWN_SETTLE_LIMIT: Duration = Duration::from_secs(1);
/// How often that wait re-checks.
const TEARDOWN_SETTLE_POLL: Duration = Duration::from_millis(50);

/// One direction of the running unit, as the watch loop sees it.
#[derive(Clone, Copy, Debug)]
struct Observed {
    /// The device the unit is using.
    bound: u32,
    /// Whether that device still exists.
    alive: bool,
    /// True when no name is configured, so the direction follows the system
    /// default; false when a name pins it.
    follows_default: bool,
    /// The system default right now (`None` when there is none). Only
    /// consulted when `follows_default`.
    default_now: Option<u32>,
}

/// Whether one direction of the running unit is out of date: its device has
/// died, or it follows the default and the default is now something else (or
/// nothing).
///
/// A pinned direction rebuilds only when its device dies. This is
/// deliberately not "does the name still resolve to `bound`": while a VPIO
/// unit runs, every output device in this process also shows an input stream
/// (the unit's echo-reference taps), so re-resolving a pinned input name
/// mid-run can find a speaker sharing the name and rebuild every check. The
/// trade-off: plugging in a second device that also matches a pinned name no
/// longer switches to it — the bound device is kept until it goes away.
fn direction_stale(
    bound: u32,
    alive: bool,
    follows_default: bool,
    default_now: Option<u32>,
) -> bool {
    !alive || (follows_default && default_now != Some(bound))
}

/// Whether the unit must be rebuilt: either direction is stale, see
/// [`direction_stale`].
fn should_rebuild(input: Observed, output: Observed) -> bool {
    [input, output]
        .into_iter()
        .any(|d| direction_stale(d.bound, d.alive, d.follows_default, d.default_now))
}

/// Read one bound direction's state for [`should_rebuild`], without
/// resolving any names. The default is only queried when it matters.
fn observe(bound: u32, dir: Direction, pinned: bool) -> Observed {
    Observed {
        bound,
        alive: device::is_alive(bound),
        follows_default: !pinned,
        default_now: if pinned {
            None
        } else {
            device::default_device(dir)
        },
    }
}

/// Before resolving names to open a unit, wait until `aggregate_present`
/// reports that no voice-processing unit's private aggregate is listed,
/// polling at most every `poll` for at most `limit`, so names are never
/// resolved while a disposed unit's echo-reference taps are still in the
/// device list. That unit may be this backend's own (a rebuild or backoff
/// attempt) or a previous backend's in the same process (the app restarts
/// the assistant by dropping one backend and opening another).
///
/// `Some(true)`: settled. `Some(false)`: still present at `limit`; the caller
/// resolves anyway. `None`: a stop arrived — return promptly. A change
/// notification does not shorten the poll interval, so a burst of them
/// cannot turn this into back-to-back device listings.
fn await_teardown(
    wake: &Receiver<Wake>,
    limit: Duration,
    poll: Duration,
    aggregate_present: impl FnMut() -> bool,
) -> Option<bool> {
    wait_while(wake, limit, poll, aggregate_present)
}

/// Poll `pending` at most every `poll` until it reports false or `limit`
/// passes, waking for nothing but a stop. `Some(true)`: done. `Some(false)`:
/// still pending at `limit`. `None`: a stop arrived. The shape of every
/// bounded, Stop-aware wait before opening a unit — see [`await_teardown`].
fn wait_while(
    wake: &Receiver<Wake>,
    limit: Duration,
    poll: Duration,
    mut pending: impl FnMut() -> bool,
) -> Option<bool> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if !pending() {
            return Some(true);
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Some(false);
        }
        if !sleep_or_stop(wake, poll.min(left)) {
            return None;
        }
    }
}

/// Park for the whole of `delay` unless stopped: unlike [`wait_or_stop`], a
/// `Changed` wake is consumed and the wait goes on. False means stop.
fn sleep_or_stop(rx: &Receiver<Wake>, delay: Duration) -> bool {
    let deadline = std::time::Instant::now() + delay;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        if !wait_or_stop(rx, left) {
            return false;
        }
    }
}

/// Park for `delay` or until woken. False means stop.
fn wait_or_stop(rx: &Receiver<Wake>, delay: Duration) -> bool {
    match rx.recv_timeout(delay) {
        Ok(Wake::Stop) | Err(RecvTimeoutError::Disconnected) => false,
        Ok(Wake::Changed) | Err(RecvTimeoutError::Timeout) => true,
    }
}

/// The configured device names, owned for the supervisor thread. Resolved
/// only when opening a unit; while one runs, `Some` just means "pinned" —
/// see [`direction_stale`].
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

/// A running unit and the device rates raised for it. Fields drop in
/// declaration order, so the unit — which holds the devices — is disposed
/// before the guard puts their rates back.
struct Running {
    voice: VoiceUnit,
    _rates: RateGuard,
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
        if self.processing_rate_hz != 0 && self.processing_rate_hz < FULL_VOICE_RATE_HZ {
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
/// system default, including when it changes later. A pinned device is kept
/// until it disappears: a second matching device plugged in later is not
/// switched to (see [`direction_stale`] for why).
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

/// Names for a settings UI's input picker, straight from CoreAudio: real
/// microphones only — not a running voice-processing unit's echo-reference
/// taps on output devices, nor its private aggregate — each name once.
///
/// cpal's list cannot be used for this on macOS while voice processing runs
/// in the same process: it shows every speaker as an input too.
pub fn list_input_device_names() -> Result<Vec<String>, AudioError> {
    device::list_names(Direction::Input)
}

/// Names for a settings UI's output picker: every output device except a
/// running voice-processing unit's private aggregate, each name once.
pub fn list_output_device_names() -> Result<Vec<String>, AudioError> {
    device::list_names(Direction::Output)
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
    // Whether the teardown wait has already been reported unsettled since
    // the last successful open.
    let mut settle_logged = false;
    // Devices whose rate could not be raised, each reported once.
    let mut rate_refused = Vec::new();

    loop {
        // Before every resolve, the first one included: a previous backend in
        // this process may have just dropped its unit. On a clean system this
        // is one device-list read.
        match await_teardown(
            &wake,
            TEARDOWN_SETTLE_LIMIT,
            TEARDOWN_SETTLE_POLL,
            device::vpio_aggregate_present,
        ) {
            None => return,
            Some(true) => {}
            Some(false) => {
                if !settle_logged {
                    eprintln!(
                        "uia-audio: coreaudio voice-processing teardown did not settle \
                         within 1 s; resolving anyway"
                    );
                    settle_logged = true;
                }
            }
        }
        // Resolution reads the unfiltered device list on purpose: some real
        // mics report terminal type 0, so filtering taps by terminal type
        // here could hide them. The wait above is what keeps taps out.
        let opened = match wanted.resolve() {
            Ok((input, output)) => {
                // Raised after resolving and before opening: VPIO picks its
                // processing rate from the devices' rates when it opens.
                let Some(rates) = RateGuard::raise(&wake, &[&input, &output], &mut rate_refused)
                else {
                    return;
                };
                // On failure `rates` drops here, putting the rates back
                // before the backoff below.
                VoiceUnit::open(input.id, output.id, &shared, &frames_tx).map(|voice| {
                    (
                        Running {
                            voice,
                            _rates: rates,
                        },
                        input,
                        output,
                    )
                })
            }
            Err(e) => Err(e),
        };
        let (running, input, output) = match opened {
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
            processing_rate_hz: running.voice.processing_rate_hz,
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
        settle_logged = false;

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
            // Never `wanted.resolve()` here: the unit is running, so the
            // device list includes its echo-reference taps.
            let input = observe(bound.0, Direction::Input, wanted.input.is_some());
            let output = observe(bound.1, Direction::Output, wanted.output.is_some());
            if should_rebuild(input, output) {
                eprintln!(
                    "uia-audio: coreaudio audio devices changed; rebuilding voice processing"
                );
                break;
            }
        }
        drop(listeners);
        // The unit first, then its raised rates: see `Running`.
        drop(running);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const MIC: u32 = 90;
    const SPEAKER: u32 = 107;

    /// A healthy direction as the watch loop sees it: alive, and (when it
    /// follows the default) the default still is the bound device.
    fn healthy(bound: u32, follows_default: bool) -> Observed {
        Observed {
            bound,
            alive: true,
            follows_default,
            default_now: Some(bound),
        }
    }

    /// Review focus 1: notification bursts with nothing actually changed
    /// must not tear the unit down — pinned or following the default.
    #[test]
    fn unchanged_bound_devices_never_rebuild() {
        for follows in [true, false] {
            assert!(!should_rebuild(
                healthy(MIC, follows),
                healthy(SPEAKER, follows)
            ));
        }
    }

    #[test]
    fn a_dead_device_rebuilds() {
        for follows in [true, false] {
            let dead_mic = Observed {
                alive: false,
                ..healthy(MIC, follows)
            };
            let dead_speaker = Observed {
                alive: false,
                ..healthy(SPEAKER, follows)
            };
            assert!(should_rebuild(dead_mic, healthy(SPEAKER, follows)));
            assert!(should_rebuild(healthy(MIC, follows), dead_speaker));
        }
    }

    /// The whole point of following the default: when it moves, rebuild.
    #[test]
    fn a_moved_default_rebuilds_an_unpinned_direction() {
        let moved_output = Observed {
            default_now: Some(81),
            ..healthy(SPEAKER, true)
        };
        let moved_input = Observed {
            default_now: Some(76),
            ..healthy(MIC, true)
        };
        assert!(should_rebuild(healthy(MIC, true), moved_output));
        assert!(should_rebuild(moved_input, healthy(SPEAKER, true)));
    }

    /// No default at all goes through the reopen backoff, which reports
    /// it, rather than keeping a device the system no longer offers.
    #[test]
    fn no_default_rebuilds_an_unpinned_direction() {
        let gone = Observed {
            default_now: None,
            ..healthy(SPEAKER, true)
        };
        assert!(should_rebuild(healthy(MIC, true), gone));
    }

    /// A pinned name keeps its device until it dies, whatever the default
    /// does — the watch loop never re-resolves names while VPIO's echo
    /// reference taps are visible.
    #[test]
    fn a_pinned_direction_ignores_a_moved_default() {
        let moved = Observed {
            default_now: Some(81),
            ..healthy(SPEAKER, false)
        };
        assert!(!should_rebuild(healthy(MIC, false), moved));
        assert!(!direction_stale(SPEAKER, true, false, Some(81)));
        // ...but still rebuilds once it dies.
        assert!(direction_stale(SPEAKER, false, false, Some(SPEAKER)));
    }

    #[test]
    fn a_pinned_direction_ignores_no_default() {
        let none = Observed {
            default_now: None,
            ..healthy(MIC, false)
        };
        assert!(!should_rebuild(none, healthy(SPEAKER, false)));
        assert!(!direction_stale(MIC, true, false, None));
    }

    /// Nothing to wait for: the teardown has already settled.
    #[test]
    fn a_settled_teardown_does_not_wait() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let got = await_teardown(
            &rx,
            Duration::from_secs(1),
            Duration::from_millis(50),
            || false,
        );
        assert_eq!(got, Some(true));
        assert!(started.elapsed() < Duration::from_millis(40));
    }

    /// The aggregate goes away after a few polls: settled, and no longer
    /// than it took.
    #[test]
    fn a_teardown_that_settles_late_is_waited_for() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut polls = 0;
        let got = await_teardown(
            &rx,
            Duration::from_secs(1),
            Duration::from_millis(5),
            || {
                polls += 1;
                polls < 4
            },
        );
        assert_eq!(got, Some(true));
        assert_eq!(polls, 4);
    }

    /// Never settles: give up at the limit and report it, rather than
    /// blocking reopen forever.
    #[test]
    fn a_teardown_that_never_settles_gives_up_at_the_limit() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let got = await_teardown(
            &rx,
            Duration::from_millis(100),
            Duration::from_millis(10),
            || true,
        );
        assert_eq!(got, Some(false));
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(100), "took {took:?}");
        assert!(took < Duration::from_millis(500), "took {took:?}");
    }

    /// A notification burst during teardown (the aggregate leaving fires
    /// device-list changes) must not turn the poll into back-to-back
    /// listings: at most one check per poll interval.
    #[test]
    fn a_burst_of_changes_does_not_speed_up_the_teardown_poll() {
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..20 {
            tx.send(Wake::Changed).unwrap();
        }
        let mut polls = 0;
        let got = await_teardown(
            &rx,
            Duration::from_millis(100),
            Duration::from_millis(50),
            || {
                polls += 1;
                true
            },
        );
        assert_eq!(got, Some(false));
        assert!(polls <= 4, "polled {polls} times in 100 ms");
    }

    /// Dropping the backend must not wait out the settle limit, even with
    /// change notifications queued ahead of the stop.
    #[test]
    fn a_stop_behind_changes_during_the_teardown_wait_is_prompt() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Wake::Changed).unwrap();
        tx.send(Wake::Changed).unwrap();
        tx.send(Wake::Stop).unwrap();
        let started = Instant::now();
        let got = await_teardown(
            &rx,
            Duration::from_secs(5),
            Duration::from_millis(50),
            || true,
        );
        assert_eq!(got, None);
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    /// Dropping the backend must not wait out the settle limit.
    #[test]
    fn a_stop_during_the_teardown_wait_is_not_waited_out() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Wake::Stop).unwrap();
        let started = Instant::now();
        let got = await_teardown(
            &rx,
            Duration::from_secs(5),
            Duration::from_millis(50),
            || true,
        );
        assert_eq!(got, None);
        assert!(started.elapsed() < Duration::from_millis(200));
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

    /// The engine's voice is 24 kHz PCM, so processing at 24 kHz or more
    /// loses nothing and must not be called band-limited.
    #[test]
    fn a_rate_at_or_above_24k_is_not_band_limited() {
        for rate in [FULL_VOICE_RATE_HZ, 32_000, 44_100, 48_000] {
            let opened = Opened {
                input: "Webcam".to_string(),
                output: "Speakers".to_string(),
                processing_rate_hz: rate,
            };
            let text = opened.to_string();
            assert!(!text.contains("band-limited"), "got: {text}");
        }
    }

    #[test]
    fn a_known_rate_below_24k_notes_band_limiting() {
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

    /// While VPIO runs, CoreAudio shows every speaker as an input and adds a
    /// private aggregate. The Settings lists must not: no aggregate, no
    /// repeats, and no input that was not an input before the unit opened.
    #[test]
    #[ignore = "requires real audio devices; run manually on a Mac with --test-threads=1 \
                (its baseline is weakened if another VPIO test runs in parallel)"]
    fn the_device_lists_hide_voice_processing_taps_and_aggregate() {
        let baseline = list_input_device_names().unwrap();
        let (_source, _sink) = open_voice_processing(None, None).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let inputs = list_input_device_names().unwrap();
        let outputs = list_output_device_names().unwrap();

        for name in inputs.iter().chain(&outputs) {
            assert!(
                !name.contains("VPAUAggregateAudioDevice"),
                "aggregate listed: {inputs:?} / {outputs:?}"
            );
        }
        let unique: std::collections::HashSet<&String> = inputs.iter().collect();
        assert_eq!(
            unique.len(),
            inputs.len(),
            "repeated input names: {inputs:?}"
        );
        for name in &inputs {
            assert!(
                baseline.contains(name),
                "{name:?} only appears as an input while VPIO runs \
                 (baseline {baseline:?}, now {inputs:?})"
            );
        }
    }

    /// A slow mic is raised for voice processing and put back on release.
    /// Runs the real supervisor so the opened processing rate is visible.
    #[test]
    #[ignore = "requires real audio devices; run manually on a Mac with --test-threads=1; \
                set UIA_TEST_SLOW_INPUT to a mic whose rate is below 24 kHz but supports more"]
    fn a_slow_input_is_raised_while_open_and_restored_on_release() {
        let name = std::env::var("UIA_TEST_SLOW_INPUT").expect("set UIA_TEST_SLOW_INPUT");
        let mic = device::resolve(Direction::Input, Some(&name)).unwrap();
        let before = device::nominal_rate(mic.id).expect("unreadable nominal rate");
        assert!(
            before < f64::from(FULL_VOICE_RATE_HZ),
            "{name:?} already runs at {before} Hz; pick a slower mic"
        );

        let (frames_tx, _frames_rx) = tokio::sync::mpsc::channel(64);
        let shared = Arc::new(Shared::new());
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let listener_tx: &'static Sender<Wake> = Box::leak(Box::new(wake_tx.clone()));
        let wanted = Wanted {
            input: Some(name.clone()),
            output: None,
        };
        let worker = std::thread::spawn(move || {
            supervise(ready_tx, wake_rx, listener_tx, shared, frames_tx, wanted)
        });
        let opened = ready_rx
            .recv()
            .unwrap()
            .expect("voice processing did not open");
        let rate = opened.processing_rate_hz;
        wake_tx.send(Wake::Stop).unwrap();
        worker.join().unwrap();
        assert!(
            rate >= FULL_VOICE_RATE_HZ,
            "processing at {rate} Hz: {opened}"
        );

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut after = device::nominal_rate(mic.id);
        while after != Some(before) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            after = device::nominal_rate(mic.id);
        }
        assert_eq!(after, Some(before), "{name:?} was not restored");
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
