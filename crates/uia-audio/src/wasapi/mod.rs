// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
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

mod capture;
mod com;
mod render;

pub use capture::WasapiSource;
pub use render::WasapiSink;

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

/// A device's sample rate and channel count, published as one word.
///
/// Two atomics would let a reader pair the new device's rate with the previous
/// one's channel count during a reopen. The window is microseconds and nothing
/// currently reads the channel count, but "safe because nobody looks" is the
/// kind of reasoning that stops being true quietly, and packing them costs a
/// shift.
pub(crate) struct LiveFormat(std::sync::atomic::AtomicU64);

impl LiveFormat {
    pub(crate) fn new() -> Self {
        Self(std::sync::atomic::AtomicU64::new(0))
    }

    pub(crate) fn store(&self, sample_rate_hz: u32, channels: u16) {
        let packed = (u64::from(sample_rate_hz) << 16) | u64::from(channels);
        self.0.store(packed, Ordering::Relaxed);
    }

    pub(crate) fn load(&self) -> (u32, u16) {
        let packed = self.0.load(Ordering::Relaxed);
        ((packed >> 16) as u32, packed as u16)
    }
}

/// Delays between attempts to reopen a device that has gone away, in
/// milliseconds; the last repeats for as long as it takes.
///
/// The tail is deliberately patient rather than a giving-up point. The case
/// this exists for is a KVM switch or a dock: the microphone is handed to
/// another machine and comes back minutes or hours later, and the right
/// behaviour is to still be there when it does. Retrying forever at five
/// second intervals costs one device enumeration per tick; giving up costs
/// the user an app that looks fine and cannot hear them.
const REOPEN_DELAYS_MS: [u32; 5] = [200, 500, 1_000, 2_000, 5_000];

/// The delay for a given attempt, saturating at the last entry.
///
/// Its own function so the clamp is testable without a device: an
/// out-of-bounds index here would panic on the audio thread and take capture
/// down permanently, which is precisely the failure the reopen loop exists to
/// prevent. `attempt` counts up without limit for a device that never returns.
fn reopen_delay_ms(attempt: u32) -> u32 {
    REOPEN_DELAYS_MS[(attempt as usize).min(REOPEN_DELAYS_MS.len() - 1)]
}

/// Park between reopen attempts, waking early if shutdown is signalled.
///
/// Waits on the worker's own wake event rather than sleeping, so `Drop`'s
/// `SetEvent` cuts a five-second backoff short instead of making the join wait
/// it out. `None` means the very first open never succeeded, so no event
/// exists yet and there is nothing to wait on but the clock.
///
/// Returns false when the caller should stop rather than try again.
fn wait_before_reopen(event: Option<EventHandle>, stop: &Arc<AtomicBool>, attempt: u32) -> bool {
    if stop.load(Ordering::SeqCst) {
        return false;
    }
    let ms = reopen_delay_ms(attempt);
    match event {
        Some(event) => unsafe {
            WaitForSingleObject(event.0, ms);
        },
        None => std::thread::sleep(std::time::Duration::from_millis(u64::from(ms))),
    }
    !stop.load(Ordering::SeqCst)
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

    /// Rate and channels must survive a round trip together, including the
    /// rates and channel counts real endpoints actually report.
    #[test]
    fn a_live_format_round_trips_rate_and_channels() {
        let live = LiveFormat::new();
        assert_eq!(live.load(), (0, 0), "starts unset, not garbage");
        for (rate, channels) in [(48_000u32, 2u16), (44_100, 1), (16_000, 8), (192_000, 2)] {
            live.store(rate, channels);
            assert_eq!(live.load(), (rate, channels));
        }
    }

    /// The reopen loop counts attempts up without bound while a device stays
    /// away, so the delay lookup must saturate rather than index past its
    /// table. A panic here happens on the audio thread and kills capture for
    /// the life of the process — the exact failure the loop exists to fix.
    #[test]
    fn the_reopen_delay_saturates_instead_of_indexing_past_the_table() {
        assert_eq!(reopen_delay_ms(0), REOPEN_DELAYS_MS[0]);
        let last = REOPEN_DELAYS_MS[REOPEN_DELAYS_MS.len() - 1];
        for attempt in [
            REOPEN_DELAYS_MS.len() as u32 - 1,
            REOPEN_DELAYS_MS.len() as u32,
            1_000,
            u32::MAX,
        ] {
            assert_eq!(reopen_delay_ms(attempt), last, "attempt {attempt}");
        }
    }

    /// The delays must grow, or a device that is genuinely gone is enumerated
    /// at the fastest rate forever.
    #[test]
    fn the_reopen_delays_back_off() {
        assert!(
            REOPEN_DELAYS_MS.windows(2).all(|w| w[0] < w[1]),
            "delays must increase: {REOPEN_DELAYS_MS:?}"
        );
    }

    /// Shutdown must not have to wait out a backoff it arrived during. With
    /// the flag already set there is nothing to wait for, so this returns
    /// immediately and says "stop" — checked by the clock, since the whole
    /// point is that it does not park.
    #[test]
    fn a_stop_signalled_during_backoff_is_not_waited_out() {
        let stop = Arc::new(AtomicBool::new(true));
        let started = std::time::Instant::now();
        // The longest delay in the table, so a wait would be unmistakable.
        let again = wait_before_reopen(None, &stop, u32::MAX);
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

    /// A configured device name that matches nothing must be a clear error,
    /// not a silent fallback to the default — otherwise a `uia.toml` typo
    /// would look identical to the device just not existing at all.
    #[tokio::test]
    #[ignore = "requires a real audio device; run manually on Windows"]
    async fn an_unmatched_input_device_name_is_a_clear_error() {
        // `.unwrap_err()` needs `T: Debug`, which `WasapiSource` isn't (it
        // holds a raw WASAPI event handle) - match instead.
        let Err(err) =
            WasapiSource::input_communications(Some("definitely-not-a-real-device-9f3a1c"))
        else {
            panic!("expected an error for a device name that cannot exist");
        };
        assert!(
            err.to_string().contains("no audio device matching"),
            "got: {err}"
        );
    }
}
