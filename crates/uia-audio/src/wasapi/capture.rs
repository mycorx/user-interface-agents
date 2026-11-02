// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Microphone capture through WASAPI's Communications category, exposed as an
//! `AudioSource`.
//!
//! Shaped deliberately like `crate::input::CpalSource` — a bounded channel fed
//! by a thread that never blocks, drained by `next_frame` — so the two are
//! interchangeable at the `SessionDeps` boundary and only the echo
//! cancellation differs.

use super::com::{EventHandle, create_wake_event, open_client};
use super::notify::DefaultChangeWatch;
use super::{
    PumpExit, Ready, StreamCategory, WAIT_TIMEOUT_MS, request_mmcss, revert_mmcss, stop_worker,
    wait_before_reopen, watch_default,
};
use crate::follow::{self, FirstOpen, Flow, OpenFailure};
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use tokio::sync::mpsc;
use uia_core::audio::{
    AudioError, AudioFormat, AudioSource, WaitingCell, downmix_to_mono, f32_to_i16,
};
use windows::Win32::Foundation::WAIT_FAILED;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, IAudioCaptureClient, IAudioClient2, eCapture,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::WaitForSingleObject;

pub struct WasapiSource {
    /// Live rather than fixed, because the device can be replaced under us.
    /// A reopened endpoint may run at a different rate than the one that
    /// died, and the session re-reads `format()` per frame and rebuilds its
    /// resampler when the pair changes — so publishing the new rate here is
    /// the whole of what a rate change needs.
    sample_rate_hz: Arc<AtomicU32>,
    rx: mpsc::Receiver<Vec<i16>>,
    /// What the worker is waiting for while its device is missing.
    waiting: WaitingCell,
    stop: Arc<AtomicBool>,
    /// Signalled by `Drop` to wake the worker out of its wait. Closed by
    /// `Drop` too, but only after joining — never by the worker itself, so
    /// the handle cannot be closed while `SetEvent` is racing it.
    event: EventHandle,
    worker: Option<JoinHandle<()>>,
}

impl WasapiSource {
    /// Open the default communications microphone with Windows' own
    /// AEC/AGC/NS engaged.
    pub fn default_input_communications() -> Result<Self, AudioError> {
        Self::input_communications(None)
    }

    /// Open the communications microphone with Windows' own AEC/AGC/NS
    /// engaged. See [`WasapiSource::open`].
    pub fn input_communications(device_name: Option<&str>) -> Result<Self, AudioError> {
        Self::open(device_name, StreamCategory::Communications)
    }

    /// Open the microphone. `device_name`, when `Some`, overrides Windows'
    /// `eCommunications` default with the first active input device whose
    /// name contains it — see `com::find_device_by_name` for why that
    /// override exists. `category` says whether Windows' AEC/AGC/NS applies.
    ///
    /// Returns once the first open has settled, never blocking for a device
    /// to appear: a pinned name that matches nothing opens a source that
    /// waits for it, says so through [`AudioSource::waiting`], and starts
    /// capturing once it appears. An unpinned source follows the
    /// communications default when it moves.
    pub fn open(device_name: Option<&str>, category: StreamCategory) -> Result<Self, AudioError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Ready, AudioError>>();
        // Bounded and small, for the same reason `CpalSource` bounds its own:
        // this is live audio, and a consumer that has fallen behind wants the
        // newest frame dropped, not an ever-growing backlog to catch up on.
        let (frame_tx, rx) = mpsc::channel::<Vec<i16>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let sample_rate_hz = Arc::new(AtomicU32::new(0));
        let waiting = WaitingCell::default();
        let event = create_wake_event()?;
        let worker = Worker {
            ready_tx,
            frame_tx,
            stop: Arc::clone(&stop),
            device_name: device_name.map(str::to_owned),
            category,
            event,
            sample_rate_hz: Arc::clone(&sample_rate_hz),
            waiting: waiting.clone(),
        };

        let worker = match std::thread::Builder::new()
            .name("uia-wasapi-capture".to_string())
            .spawn(move || worker.thread())
        {
            Ok(worker) => worker,
            Err(e) => {
                stop_worker(&stop, event, None);
                return Err(AudioError::DeviceUnavailable(format!(
                    "wasapi: could not spawn the capture thread: {e}"
                )));
            }
        };

        // The worker sends exactly one readiness message; a disconnect here
        // means it panicked before doing so.
        let ready = match ready_rx.recv() {
            Ok(Ok(ready)) => ready,
            Ok(Err(e)) => {
                stop_worker(&stop, event, Some(worker));
                return Err(e);
            }
            Err(_) => {
                stop_worker(&stop, event, Some(worker));
                return Err(AudioError::DeviceUnavailable(
                    "wasapi: the capture thread stopped before reporting readiness".to_string(),
                ));
            }
        };

        if let Ready::Opened {
            device_name,
            sample_rate_hz,
            ..
        } = ready
        {
            eprintln!(
                "uia-audio: wasapi capture opened {device_name:?} {} ({sample_rate_hz} Hz)",
                category.describe()
            );
        }

        Ok(Self {
            sample_rate_hz,
            rx,
            waiting,
            stop,
            event,
            worker: Some(worker),
        })
    }
}

#[async_trait]
impl AudioSource for WasapiSource {
    fn format(&self) -> AudioFormat {
        // Mono regardless of what the device captures, for the same reason
        // `CpalSource` does it: every frame is downmixed, and an advertised
        // stereo format would leave the resampler and the Opus encoder
        // reading interleaved L/R as if it were one channel.
        AudioFormat::mono_pcm16(self.sample_rate_hz.load(Ordering::Relaxed))
    }

    async fn next_frame(&mut self) -> Option<Vec<i16>> {
        self.rx.recv().await
    }

    fn waiting(&self) -> Option<WaitingCell> {
        Some(self.waiting.clone())
    }
}

impl Drop for WasapiSource {
    fn drop(&mut self) {
        stop_worker(&self.stop, self.event, self.worker.take());
    }
}

/// Everything the capture thread owns.
struct Worker {
    ready_tx: std::sync::mpsc::Sender<Result<Ready, AudioError>>,
    frame_tx: mpsc::Sender<Vec<i16>>,
    stop: Arc<AtomicBool>,
    device_name: Option<String>,
    category: StreamCategory,
    event: EventHandle,
    sample_rate_hz: Arc<AtomicU32>,
    waiting: WaitingCell,
}

impl Worker {
    fn thread(self) {
        unsafe {
            if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                let _ = self
                    .ready_tx
                    .send(Err(AudioError::DeviceUnavailable(format!(
                        "wasapi: CoInitializeEx failed on the capture thread: {e}"
                    ))));
                return;
            }
            // Registered after COM is up and dropped (unregistered) before it
            // goes down: it is a COM object like any other.
            let watch = watch_default(self.device_name.as_deref(), Flow::Capture, self.event);
            self.run(watch.as_ref());
            drop(watch);
            CoUninitialize();
        }
    }

    /// Hold a capture stream open for as long as the source lives, reopening
    /// the device whenever it goes away, and for an unpinned source whenever
    /// the communications default moves.
    ///
    /// Losing the device used to end capture for good: `drain_packets` failed
    /// with `AUDCLNT_E_DEVICE_INVALIDATED`, the loop broke, `frame_tx`
    /// dropped, `next_frame()` returned `None` forever, and the session above
    /// exited with `SourceExhausted` into a log line nobody reads. The window
    /// stayed up looking healthy, the mic button still toggled, and nothing
    /// worked again until the app was restarted.
    ///
    /// That is not an edge case. A KVM switch, a dock, a USB headset moved to
    /// another machine, or Windows changing the communications default all
    /// invalidate the client, and the device usually comes back.
    ///
    /// Only a *first* open that fails for a reason other than a missing
    /// pinned device is fatal. It is the one the constructor is blocked on,
    /// and a microphone that cannot be opened at startup is a real error the
    /// user should be told about rather than a device in transit. A missing
    /// pinned device is waited for instead, as every later failure is.
    unsafe fn run(&self, watch: Option<&DefaultChangeWatch>) {
        unsafe {
            let pinned = self.device_name.as_deref();
            // `Some` until readiness has been reported. Taking it is what
            // marks every later attempt as a reopen rather than a first open.
            let mut ready_tx = Some(&self.ready_tx);
            let mut attempt = 0u32;
            // So a device that flaps does not reprint the same line every
            // retry.
            let mut loss_logged = false;
            // Whether to back off before the next open: not before the first,
            // nor after a default change.
            let mut backoff = false;

            loop {
                if backoff {
                    if !wait_before_reopen(self.event, &self.stop, attempt) {
                        return;
                    }
                    attempt = attempt.saturating_add(1);
                }
                if self.stop.load(Ordering::SeqCst) {
                    return;
                }
                // Whatever the default is now is what this open resolves, so
                // a change seen before it needs no second reopen.
                if let Some(watch) = watch {
                    watch.take_changed();
                }
                let stream = match open_capture_stream(pinned, self.event, self.category) {
                    Ok(stream) => stream,
                    Err(failure) => {
                        backoff = true;
                        if let Some(tx) = ready_tx.take() {
                            match follow::first_open(&failure) {
                                FirstOpen::Wait(what) => {
                                    eprintln!(
                                        "uia-audio: wasapi capture is waiting: {}",
                                        AudioError::from(failure)
                                    );
                                    loss_logged = true;
                                    self.waiting.set(Some(what));
                                    if tx.send(Ok(Ready::Waiting)).is_err() {
                                        return;
                                    }
                                }
                                FirstOpen::Fail => {
                                    let _ = tx.send(Err(failure.into()));
                                    return;
                                }
                            }
                            continue;
                        }
                        if !loss_logged {
                            eprintln!(
                                "uia-audio: wasapi capture cannot reopen ({}); still trying",
                                AudioError::from(failure)
                            );
                            loss_logged = true;
                        }
                        self.waiting.set(Some(follow::waiting_for(pinned)));
                        continue;
                    }
                };
                self.waiting.set(None);
                // Published before readiness so the constructor never reads a
                // zero, and updated on every reopen so a replacement device at
                // a different rate is picked up by the session's resampler.
                self.sample_rate_hz
                    .store(stream.sample_rate_hz, Ordering::Relaxed);

                match ready_tx.take() {
                    Some(tx) => {
                        if tx
                            .send(Ok(Ready::Opened {
                                device_name: stream.device_name.clone(),
                                sample_rate_hz: stream.sample_rate_hz,
                                channels: stream.channels,
                            }))
                            .is_err()
                        {
                            // The constructor gave up; unwind rather than
                            // capture into a void.
                            let _ = stream.client.Stop();
                            return;
                        }
                    }
                    None => eprintln!(
                        "uia-audio: wasapi capture reopened {:?} ({} Hz)",
                        stream.device_name, stream.sample_rate_hz
                    ),
                }
                attempt = 0;
                loss_logged = false;

                let mmcss = request_mmcss("capture");
                let exit = self.pump(&stream, watch);
                revert_mmcss(mmcss);
                let _ = stream.client.Stop();

                match exit {
                    PumpExit::Stopped => return,
                    PumpExit::DefaultChanged => {
                        eprintln!("uia-audio: wasapi capture default changed; reopening");
                        backoff = false;
                    }
                    PumpExit::Lost(reason) => {
                        // 0x88890004 is AUDCLNT_E_DEVICE_INVALIDATED, by far
                        // the common one: the endpoint was removed or
                        // reconfigured underneath us.
                        eprintln!(
                            "uia-audio: wasapi capture lost the device ({reason}); reopening"
                        );
                        backoff = true;
                    }
                }
            }
        }
    }

    /// Drain the stream until it fails, the default moves, or `stop` is set.
    unsafe fn pump(&self, stream: &Stream, watch: Option<&DefaultChangeWatch>) -> PumpExit {
        unsafe {
            while !self.stop.load(Ordering::SeqCst) {
                if WaitForSingleObject(self.event.0, WAIT_TIMEOUT_MS) == WAIT_FAILED {
                    return PumpExit::Lost("wait failed".to_string());
                }
                // Checked before draining as well as in the loop condition:
                // `Drop` signals the event precisely to break this wait early.
                if self.stop.load(Ordering::SeqCst) {
                    break;
                }
                if watch.is_some_and(DefaultChangeWatch::take_changed) {
                    return PumpExit::DefaultChanged;
                }
                if let Err(e) = drain_packets(&stream.capture, stream.channels, &self.frame_tx) {
                    return PumpExit::Lost(e.to_string());
                }
            }
            PumpExit::Stopped
        }
    }
}

/// An opened, started capture stream, ready to be drained.
struct Stream {
    client: IAudioClient2,
    capture: IAudioCaptureClient,
    channels: u16,
    sample_rate_hz: u32,
    device_name: String,
}

/// Open and start one capture stream.
///
/// Every failure past resolving the device means the same thing to the
/// caller — the device is not usable right now — so they are funnelled into
/// one `Result` rather than each deciding for itself whether to report or
/// retry.
unsafe fn open_capture_stream(
    device_name: Option<&str>,
    event: EventHandle,
    category: StreamCategory,
) -> Result<Stream, OpenFailure> {
    unsafe {
        let opened = open_client(eCapture, device_name, event, category)?;
        let capture: IAudioCaptureClient = opened.client.GetService().map_err(|e| {
            OpenFailure::Other(AudioError::DeviceUnavailable(format!(
                "wasapi: could not get the capture service: {e}"
            )))
        })?;
        opened.client.Start().map_err(|e| {
            OpenFailure::Other(AudioError::DeviceUnavailable(format!(
                "wasapi: could not start capture: {e}"
            )))
        })?;
        Ok(Stream {
            client: opened.client,
            capture,
            channels: opened.channels,
            sample_rate_hz: opened.sample_rate_hz,
            device_name: opened.device_name,
        })
    }
}

/// Drain every packet WASAPI currently holds, converting each to one mono
/// `i16` frame on our channel.
unsafe fn drain_packets(
    capture: &IAudioCaptureClient,
    channels: u16,
    frame_tx: &mpsc::Sender<Vec<i16>>,
) -> windows::core::Result<()> {
    unsafe {
        loop {
            let packet_frames = capture.GetNextPacketSize()?;
            if packet_frames == 0 {
                return Ok(());
            }

            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;

            // WASAPI may hand back a buffer whose contents are undefined and
            // flag it silent rather than bothering to zero it.
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let mono = if silent || data.is_null() {
                vec![0i16; frames as usize]
            } else {
                let interleaved = std::slice::from_raw_parts(
                    data as *const f32,
                    (frames * channels as u32) as usize,
                );
                downmix_to_mono(&f32_to_i16(interleaved), channels)
            };

            // Release before the send: holding a WASAPI buffer across anything
            // else is how you stall the audio engine.
            capture.ReleaseBuffer(frames)?;

            // `try_send`, never a blocking or awaited send. This is what keeps
            // `Drop`'s join deadlock-free: a full channel (slow consumer, or a
            // consumer that has stopped reading entirely) must never be able to
            // park this thread, or it would never observe the stop flag.
            let _ = frame_tx.try_send(mono);
        }
    }
}
