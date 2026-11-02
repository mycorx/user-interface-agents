// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Speaker playback through WASAPI's Communications category, exposed as an
//! `AudioSink`.
//!
//! The render side matters as much as the capture side for echo cancellation:
//! Windows cancels what it knows it is playing, so the assistant's voice has
//! to go out through a Communications-category stream for the OS to recognise
//! it as the far-end reference.
//!
//! Shaped like `crate::output::CpalSink` — a shared ring buffer drained by the
//! audio thread, padded with silence when starved — so `clear()` keeps the same
//! synchronous, non-blocking barge-in behaviour.

use super::com::{EventHandle, create_wake_event, open_client};
use super::notify::DefaultChangeWatch;
use super::{
    PumpExit, Ready, StreamCategory, WAIT_TIMEOUT_MS, request_mmcss, revert_mmcss, stop_worker,
    wait_before_reopen, watch_default,
};
use crate::follow::{self, FirstOpen, Flow, OpenFailure};
use crate::input::to_audio_format;
use crate::level::PlaybackLevel;
use crate::live::LiveFormat;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use uia_core::audio::{AudioError, AudioFormat, AudioSink, WaitingCell};
use windows::Win32::Foundation::WAIT_FAILED;
use windows::Win32::Media::Audio::{IAudioClient2, IAudioRenderClient, eRender};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::WaitForSingleObject;

pub struct WasapiSink {
    /// Live rather than fixed, for the same reason `WasapiSource`'s rate is:
    /// the device can be replaced under us, and the endpoint that comes back
    /// may differ from the one that went away. The session re-reads `format()`
    /// per frame and rebuilds its resampler when the rate pair changes.
    ///
    /// Reports the device's own channel count, matching `CpalSink`. Note the
    /// queue underneath is mono either way — `fill` fans each queued sample
    /// across the frame — so this describes the endpoint rather than what
    /// `write` accepts. That mismatch predates device recovery and is left
    /// exactly as it was rather than quietly changed here.
    format: Arc<LiveFormat>,
    buf: Arc<Mutex<VecDeque<i16>>>,
    level: PlaybackLevel,
    /// What the worker is waiting for while its device is missing.
    waiting: WaitingCell,
    stop: Arc<AtomicBool>,
    event: EventHandle,
    worker: Option<JoinHandle<()>>,
}

impl WasapiSink {
    /// Open the default communications speaker, so that what we play is what
    /// Windows cancels out of the microphone.
    pub fn default_output_communications() -> Result<Self, AudioError> {
        Self::output_communications(None)
    }

    /// Open the communications speaker, so that what we play is what Windows
    /// cancels out of the microphone. See [`WasapiSink::open`].
    pub fn output_communications(device_name: Option<&str>) -> Result<Self, AudioError> {
        Self::open(device_name, StreamCategory::Communications)
    }

    /// Open the speaker. `device_name`, when `Some`, overrides Windows'
    /// `eCommunications` default with the first active output device whose
    /// name contains it — see `com::find_device_by_name` for why that
    /// override exists. `category` says whether this stream is the far-end
    /// reference for Windows' echo cancellation.
    ///
    /// Returns once the first open has settled, never blocking for a device
    /// to appear: a pinned name that matches nothing opens a sink that waits
    /// for it, says so through [`AudioSink::waiting`], drops what it is given
    /// meanwhile, and starts playing once it appears. An unpinned sink follows
    /// the communications default when it moves.
    pub fn open(device_name: Option<&str>, category: StreamCategory) -> Result<Self, AudioError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Ready, AudioError>>();
        let buf: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let level = PlaybackLevel::default();
        let stop = Arc::new(AtomicBool::new(false));
        let format = Arc::new(LiveFormat::new());
        let waiting = WaitingCell::default();
        let event = create_wake_event()?;
        let worker = Worker {
            ready_tx,
            buf: Arc::clone(&buf),
            level: level.clone(),
            stop: Arc::clone(&stop),
            device_name: device_name.map(str::to_owned),
            category,
            event,
            format: Arc::clone(&format),
            waiting: waiting.clone(),
        };

        let worker = match std::thread::Builder::new()
            .name("uia-wasapi-render".to_string())
            .spawn(move || worker.thread())
        {
            Ok(worker) => worker,
            Err(e) => {
                stop_worker(&stop, event, None);
                return Err(AudioError::DeviceUnavailable(format!(
                    "wasapi: could not spawn the render thread: {e}"
                )));
            }
        };

        let ready = match ready_rx.recv() {
            Ok(Ok(ready)) => ready,
            Ok(Err(e)) => {
                stop_worker(&stop, event, Some(worker));
                return Err(e);
            }
            Err(_) => {
                stop_worker(&stop, event, Some(worker));
                return Err(AudioError::DeviceUnavailable(
                    "wasapi: the render thread stopped before reporting readiness".to_string(),
                ));
            }
        };

        if let Ready::Opened {
            device_name,
            sample_rate_hz,
            channels,
        } = ready
        {
            eprintln!(
                "uia-audio: wasapi render opened {device_name:?} {} ({sample_rate_hz} Hz, \
                 {channels} ch)",
                category.describe()
            );
        }

        Ok(Self {
            format,
            buf,
            level,
            waiting,
            stop,
            event,
            worker: Some(worker),
        })
    }

    /// Number of samples currently queued for playback. Exposed for tests;
    /// production code has no need to poll it.
    pub fn buffered_len(&self) -> usize {
        self.buf.lock().unwrap().len()
    }
}

#[async_trait]
impl AudioSink for WasapiSink {
    fn format(&self) -> AudioFormat {
        let (sample_rate_hz, channels) = self.format.load();
        to_audio_format(sample_rate_hz, channels)
    }

    /// Dropped while waiting for a device: nothing would play it, and a
    /// backlog would come out late, at whatever rate the device turns up at.
    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError> {
        if self.waiting.get().is_none() {
            self.buf.lock().unwrap().extend(frame.iter().copied());
        }
        Ok(())
    }

    fn clear(&mut self) {
        // Synchronous and non-blocking, per the `AudioSink` contract: barge-in
        // must not await. Note this drops only what we still hold — anything
        // already handed to WASAPI plays out regardless, which is exactly why
        // `BUFFER_DURATION_HNS` is 20 ms rather than the spikes' 200 ms.
        self.buf.lock().unwrap().clear();
    }

    fn level(&self) -> f32 {
        self.level.get()
    }

    fn waiting(&self) -> Option<WaitingCell> {
        Some(self.waiting.clone())
    }
}

impl Drop for WasapiSink {
    fn drop(&mut self) {
        stop_worker(&self.stop, self.event, self.worker.take());
    }
}

/// Everything the render thread owns.
struct Worker {
    ready_tx: std::sync::mpsc::Sender<Result<Ready, AudioError>>,
    buf: Arc<Mutex<VecDeque<i16>>>,
    level: PlaybackLevel,
    stop: Arc<AtomicBool>,
    device_name: Option<String>,
    category: StreamCategory,
    event: EventHandle,
    format: Arc<LiveFormat>,
    waiting: WaitingCell,
}

impl Worker {
    fn thread(self) {
        unsafe {
            if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                let _ = self
                    .ready_tx
                    .send(Err(AudioError::DeviceUnavailable(format!(
                        "wasapi: CoInitializeEx failed on the render thread: {e}"
                    ))));
                return;
            }
            // Registered after COM is up and dropped (unregistered) before it
            // goes down: it is a COM object like any other.
            let watch = watch_default(self.device_name.as_deref(), Flow::Render, self.event);
            self.run(watch.as_ref());
            drop(watch);
            CoUninitialize();
        }
    }

    /// Mark the sink as waiting for its device, dropping what is queued: it
    /// would come out late, at whatever rate the device turns up at.
    fn wait_for(&self, what: uia_core::audio::WaitingFor) {
        self.waiting.set(Some(what));
        self.buf.lock().unwrap().clear();
    }

    /// Hold a render stream open for as long as the sink lives, reopening the
    /// device whenever it goes away, and for an unpinned sink whenever the
    /// communications default moves.
    ///
    /// The mirror of the capture worker's `run`, and it matters for the same
    /// reason plus one more: Windows cancels from the microphone what it
    /// knows it is playing, so a dead render stream does not merely mute the
    /// assistant, it takes the echo cancellation reference with it. See that
    /// function for why losing a device is an ordinary event rather than an
    /// edge case.
    ///
    /// The queued audio survives a reopen at the same rate. `buf` is owned by
    /// the sink, not by the stream, so a device swap mid-sentence resumes
    /// rather than truncating — though whatever WASAPI had already accepted
    /// is gone with the old endpoint.
    unsafe fn run(&self, watch: Option<&DefaultChangeWatch>) {
        unsafe {
            let pinned = self.device_name.as_deref();
            let mut ready_tx = Some(&self.ready_tx);
            let mut attempt = 0u32;
            let mut loss_logged = false;
            let mut backoff = false;
            // The rate the queue was last filled for; 0 before any open.
            let mut last_rate = 0u32;

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
                let stream = match open_render_stream(
                    pinned,
                    self.event,
                    self.category,
                    last_rate,
                    &self.buf,
                    &self.level,
                ) {
                    Ok(stream) => stream,
                    Err(failure) => {
                        backoff = true;
                        if let Some(tx) = ready_tx.take() {
                            match follow::first_open(&failure) {
                                FirstOpen::Wait(what) => {
                                    eprintln!(
                                        "uia-audio: wasapi render is waiting: {}",
                                        AudioError::from(failure)
                                    );
                                    loss_logged = true;
                                    self.wait_for(what);
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
                                "uia-audio: wasapi render cannot reopen ({}); still trying",
                                AudioError::from(failure)
                            );
                            loss_logged = true;
                        }
                        self.wait_for(follow::waiting_for(pinned));
                        continue;
                    }
                };
                self.waiting.set(None);
                last_rate = stream.sample_rate_hz;
                // Published before readiness so the constructor never reads a
                // zero, and updated on every reopen so a replacement endpoint
                // is what the session resamples towards.
                self.format.store(stream.sample_rate_hz, stream.channels);

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
                            let _ = stream.client.Stop();
                            return;
                        }
                    }
                    None => eprintln!(
                        "uia-audio: wasapi render reopened {:?} ({} Hz, {} ch)",
                        stream.device_name, stream.sample_rate_hz, stream.channels
                    ),
                }
                attempt = 0;
                loss_logged = false;

                let mmcss = request_mmcss("render");
                let exit = self.pump(&stream, watch);
                revert_mmcss(mmcss);
                let _ = stream.client.Stop();

                match exit {
                    PumpExit::Stopped => return,
                    PumpExit::DefaultChanged => {
                        eprintln!("uia-audio: wasapi render default changed; reopening");
                        backoff = false;
                    }
                    PumpExit::Lost(reason) => {
                        eprintln!("uia-audio: wasapi render lost the device ({reason}); reopening");
                        backoff = true;
                    }
                }
            }
        }
    }

    /// Feed the stream until it fails, the default moves, or `stop` is set.
    unsafe fn pump(&self, stream: &Stream, watch: Option<&DefaultChangeWatch>) -> PumpExit {
        unsafe {
            while !self.stop.load(Ordering::SeqCst) {
                if WaitForSingleObject(self.event.0, WAIT_TIMEOUT_MS) == WAIT_FAILED {
                    return PumpExit::Lost("wait failed".to_string());
                }
                if self.stop.load(Ordering::SeqCst) {
                    break;
                }
                if watch.is_some_and(DefaultChangeWatch::take_changed) {
                    return PumpExit::DefaultChanged;
                }
                if let Err(e) = fill(
                    &stream.client,
                    &stream.render,
                    stream.channels,
                    &self.buf,
                    &self.level,
                ) {
                    return PumpExit::Lost(e.to_string());
                }
            }
            PumpExit::Stopped
        }
    }
}

/// An opened, pre-rolled, started render stream.
struct Stream {
    client: IAudioClient2,
    render: IAudioRenderClient,
    channels: u16,
    sample_rate_hz: u32,
    device_name: String,
}

/// Open, pre-roll and start one render stream. As on the capture side, every
/// failure past resolving the device means the same thing to the caller, so
/// they share one `Result`.
///
/// `last_rate` is the rate the queue was filled for. Samples queued for
/// another rate would play at the wrong pitch, so they are dropped before
/// the pre-roll can play any of them.
unsafe fn open_render_stream(
    device_name: Option<&str>,
    event: EventHandle,
    category: StreamCategory,
    last_rate: u32,
    buf: &Arc<Mutex<VecDeque<i16>>>,
    level: &PlaybackLevel,
) -> Result<Stream, OpenFailure> {
    unsafe {
        let opened = open_client(eRender, device_name, event, category)?;
        let render: IAudioRenderClient = opened.client.GetService().map_err(|e| {
            OpenFailure::Other(AudioError::DeviceUnavailable(format!(
                "wasapi: could not get the render service: {e}"
            )))
        })?;
        if !follow::keep_queue_on_reopen(last_rate, opened.sample_rate_hz) {
            buf.lock().unwrap().clear();
        }
        // Pre-roll before starting, so the engine always has something valid
        // to play rather than whatever was in the buffer.
        fill(&opened.client, &render, opened.channels, buf, level).map_err(|e| {
            OpenFailure::Other(AudioError::DeviceUnavailable(format!(
                "wasapi: could not pre-fill the render buffer: {e}"
            )))
        })?;
        opened.client.Start().map_err(|e| {
            OpenFailure::Other(AudioError::DeviceUnavailable(format!(
                "wasapi: could not start playback: {e}"
            )))
        })?;
        Ok(Stream {
            client: opened.client,
            render,
            channels: opened.channels,
            sample_rate_hz: opened.sample_rate_hz,
            device_name: opened.device_name,
        })
    }
}

/// Top up whatever space the device buffer currently has, from our queue.
unsafe fn fill(
    client: &IAudioClient2,
    render: &IAudioRenderClient,
    channels: u16,
    buf: &Arc<Mutex<VecDeque<i16>>>,
    level: &PlaybackLevel,
) -> windows::core::Result<()> {
    unsafe {
        let available = client
            .GetBufferSize()?
            .saturating_sub(client.GetCurrentPadding()?);
        if available == 0 {
            return Ok(());
        }

        let ptr = render.GetBuffer(available)?;
        let slots =
            std::slice::from_raw_parts_mut(ptr as *mut f32, (available * channels as u32) as usize);

        {
            let mut queue = buf.lock().unwrap();
            // One queued *mono* sample per output frame, fanned across every
            // slot in that frame. Popping one per slot instead would play two
            // mono samples across L/R — halving the duration and pitching the
            // voice up, exactly like an unresampled rate mismatch.
            for frame in slots.chunks_mut(channels as usize) {
                let sample = match queue.pop_front() {
                    Some(s) => s as f32 / 32768.0,
                    // Starved: pad with silence rather than block the engine.
                    None => 0.0,
                };
                for slot in frame {
                    *slot = sample;
                }
            }
        }
        level.record(slots);

        render.ReleaseBuffer(available, 0)
    }
}
