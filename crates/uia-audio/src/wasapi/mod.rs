// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Windows-native echo cancellation, via WASAPI's Communications stream
//! category.
//!
//! Windows applies its own AEC/AGC/NS to any stream opened with
//! `AudioCategory_Communications` against the `eCommunications` device role.
//! That is the whole mechanism: no bundled C++, no `webrtc-audio-processing`,
//! no meson/ninja/libclang toolchain — which is what makes this viable on
//! Windows where [`crate::aec`] is not (see `.plan/LEDGER.md`: its
//! `webrtc-audio-processing-sys` dependency does not compile under MSVC at
//! all).
//!
//! The two paths are alternatives, never composed. Running AEC3 over audio
//! Windows has already cancelled beneath the app would be filtering twice;
//! `uia-app`'s `build_session` picks exactly one at compile time.
//!
//! Grown out of the two spikes in `examples/wasapi_aec_*.rs`, with the
//! corrections a long-lived backend needs and a fixed-duration diagnostic does
//! not: a real shutdown protocol, MMCSS priority, a barge-in-sized buffer, and
//! format validation at open time rather than per packet.
//!
//! The same workers also serve the plain path (echo cancellation off), opened
//! with [`StreamCategory::Plain`]: on Windows "Default" means the default
//! communication device either way, and cpal would follow the console
//! default instead.

mod capture;
mod com;
mod notify;
mod render;

pub use capture::WasapiSource;
pub use render::WasapiSink;

use crate::backoff::reopen_delay_ms;
use com::EventHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, SetEvent, WaitForSingleObject,
};
use windows::core::w;

/// Bound on how long a worker parks before re-checking its stop flag.
///
/// Only a backstop: `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` fires roughly every
/// device period (~10 ms) while audio flows, and shutdown signals the same
/// event explicitly, so this timeout is what covers a stream that has gone
/// silent *and* somehow missed the shutdown signal — not the normal path.
const WAIT_TIMEOUT_MS: u32 = 2000;

/// How a stream is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamCategory {
    /// `AudioCategory_Communications`: Windows applies its own AEC/AGC/NS.
    Communications,
    /// An ordinary stream on the same endpoint, for when echo cancellation
    /// is off.
    Plain,
}

impl StreamCategory {
    /// For the opened-device log line.
    fn describe(self) -> &'static str {
        match self {
            StreamCategory::Communications => {
                "under AudioCategory_Communications (OS echo cancellation active)"
            }
            StreamCategory::Plain => "as a plain stream (no OS echo cancellation)",
        }
    }
}

/// What a worker reports back once its first open has settled, so that a
/// failure surfaces from the constructor rather than as a port that silently
/// never works.
enum Ready {
    Opened {
        device_name: String,
        sample_rate_hz: u32,
        channels: u16,
    },
    /// A pinned device is missing; the worker keeps trying.
    Waiting,
}

/// Why a worker left its pump loop.
enum PumpExit {
    /// `stop` was set: done for good.
    Stopped,
    /// The communications default moved: reopen at once, no backoff.
    DefaultChanged,
    /// The device failed: reopen after a backoff.
    Lost(String),
}

/// Park between reopen attempts, waking early if shutdown is signalled.
///
/// Waits on the worker's own wake event rather than sleeping, so `Drop`'s
/// `SetEvent` cuts a five-second backoff short instead of making the join wait
/// it out, and a default-device change cuts it short to reopen at once. The
/// constructor creates the event before the worker starts, so there always
/// is one, even while the first open is waiting for a device.
///
/// Returns false when the caller should stop rather than try again.
fn wait_before_reopen(event: EventHandle, stop: &Arc<AtomicBool>, attempt: u32) -> bool {
    if stop.load(Ordering::SeqCst) {
        return false;
    }
    // SAFETY: the event stays open until `stop_worker` has joined this thread.
    unsafe {
        WaitForSingleObject(event.0, reopen_delay_ms(attempt));
    }
    !stop.load(Ordering::SeqCst)
}

/// Register the default-change watch for an unpinned worker.
///
/// A pinned direction never follows the default, so it gets none. A failed
/// registration is logged and the worker carries on: the stream still works
/// and still reopens when its device is lost, it only misses the move.
fn watch_default(
    pinned: Option<&str>,
    flow: crate::follow::Flow,
    event: EventHandle,
) -> Option<notify::DefaultChangeWatch> {
    if pinned.is_some() {
        return None;
    }
    notify::DefaultChangeWatch::register(flow, event)
        .inspect_err(|e| {
            eprintln!(
                "uia-audio: wasapi could not watch the default device ({e}); \
                 it will not follow a new default"
            );
        })
        .ok()
}

/// Join the "Pro Audio" MMCSS task so the audio thread is scheduled ahead of
/// ordinary work.
///
/// Neither spike does this; cpal's own WASAPI backend does. Bypassing cpal
/// means bypassing that, and a normal-priority feeder thread starved by a
/// compile or a busy UI thread underruns the render buffer audibly. Failure is
/// not fatal — the stream still works, just without the priority boost.
fn request_mmcss(role: &str) -> Option<HANDLE> {
    let mut task_index = 0u32;
    match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("uia-audio: wasapi {role} could not raise thread priority ({e}); continuing");
            None
        }
    }
}

fn revert_mmcss(handle: Option<HANDLE>) {
    if let Some(handle) = handle {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(handle);
        }
    }
}

/// Stop a worker thread and release its wake event.
///
/// Ordering is the point: set the flag, signal the event so the worker wakes
/// out of its wait immediately rather than after `WAIT_TIMEOUT_MS`, join so it
/// is provably done touching the handle, and only then close it. The worker
/// never closes its own event, which is what removes any window where the
/// `SetEvent` here could land on a handle value the OS has already recycled.
///
/// This can only be deadlock-free because the workers never block on a full
/// channel — see the `try_send` note in [`capture`].
fn stop_worker(stop: &Arc<AtomicBool>, event: EventHandle, worker: Option<JoinHandle<()>>) {
    stop.store(true, Ordering::SeqCst);
    unsafe {
        let _ = SetEvent(event.0);
    }
    if let Some(worker) = worker {
        let _ = worker.join();
    }
    unsafe {
        let _ = CloseHandle(event.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AudioSource`/`AudioSink` are `Send` supertraits, so this is already
    /// enforced at the `impl` sites — stated explicitly because it is the one
    /// property easy to break here by accident. A raw `HANDLE` is
    /// `*mut c_void` and therefore neither `Send` nor `Sync`; holding one
    /// directly rather than through [`com::EventHandle`] silently makes these
    /// types unusable as `SessionDeps` without any obvious reason why.
    #[test]
    fn the_backends_stay_sendable_across_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<WasapiSource>();
        assert_send::<WasapiSink>();
    }

    /// Shutdown must not have to wait out a backoff it arrived during. With
    /// the flag already set there is nothing to wait for, so this returns
    /// immediately and says "stop" — checked by the clock, since the whole
    /// point is that it does not park.
    #[test]
    fn a_stop_signalled_during_backoff_is_not_waited_out() {
        let stop = Arc::new(AtomicBool::new(true));
        let event = com::create_wake_event().unwrap();
        let started = std::time::Instant::now();
        // The longest delay in the table, so a wait would be unmistakable.
        let again = wait_before_reopen(event, &stop, u32::MAX);
        unsafe {
            let _ = CloseHandle(event.0);
        }
        assert!(!again, "a set stop flag must end the loop, not delay it");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "returned in {:?}; it must not sleep",
            started.elapsed()
        );
    }

    /// A 200 ms buffer (what both example spikes use) would let a fifth of a
    /// second of already-committed audio keep playing after `clear()`, well
    /// past the 50 ms barge-in budget PLAN.md freezes.
    #[test]
    fn the_device_buffer_is_inside_the_barge_in_budget() {
        let millis = com::BUFFER_DURATION_HNS / 10_000;
        assert!(
            millis <= 50,
            "device buffer bounds how much audio survives clear(); got {millis} ms"
        );
    }

    #[tokio::test]
    #[ignore = "requires a real audio device; run manually on Windows"]
    async fn the_communications_capture_endpoint_opens() {
        use uia_core::audio::AudioSource;
        let source = WasapiSource::default_input_communications().unwrap();
        assert!(source.format().sample_rate_hz > 0);
        assert_eq!(
            source.format().channels,
            1,
            "must advertise the mono it yields"
        );
    }

    #[tokio::test]
    #[ignore = "requires a real audio device; run manually on Windows"]
    async fn the_communications_render_endpoint_opens_and_clears_synchronously() {
        use uia_core::audio::AudioSink;
        let mut sink = WasapiSink::default_output_communications().unwrap();
        assert!(sink.format().sample_rate_hz > 0);
        sink.write(&[1, 2, 3, 4]).await.unwrap();
        sink.clear();
        assert_eq!(sink.buffered_len(), 0);
    }

    /// A pinned microphone that is not plugged in is waited for, never
    /// replaced and never an error: the open returns at once, reporting what
    /// it waits for, so the session can carry on with the other direction.
    #[tokio::test]
    #[ignore = "requires the Windows audio service; run manually"]
    async fn a_pinned_communications_input_missing_at_open_waits() {
        use uia_core::audio::{AudioSource, WaitingFor};
        let name = "definitely-not-a-real-device-9f3a1c";
        let started = std::time::Instant::now();
        let source = WasapiSource::open(Some(name), StreamCategory::Communications)
            .unwrap_or_else(|e| panic!("a missing pinned device must wait, got: {e}"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the open must not block waiting for the device"
        );
        assert_eq!(
            source.waiting().expect("the source reports waiting").get(),
            Some(WaitingFor::Named(name.into()))
        );
    }

    #[tokio::test]
    #[ignore = "requires the Windows audio service; run manually"]
    async fn a_plain_render_endpoint_opens_and_clears_synchronously() {
        use uia_core::audio::AudioSink;
        let mut sink = WasapiSink::open(None, StreamCategory::Plain).unwrap();
        assert!(sink.format().sample_rate_hz > 0);
        assert_eq!(
            sink.waiting().expect("the sink reports waiting").get(),
            None
        );
        sink.write(&[1, 2, 3, 4]).await.unwrap();
        sink.clear();
        assert_eq!(sink.buffered_len(), 0);
    }
}
