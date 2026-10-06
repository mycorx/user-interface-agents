// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
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

use super::com::{EventHandle, open_communications_client};
use super::{
    LiveFormat, WAIT_TIMEOUT_MS, request_mmcss, revert_mmcss, stop_worker, wait_before_reopen,
};
use crate::input::to_audio_format;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use uia_core::audio::{AudioError, AudioFormat, AudioSink};
use windows::Win32::Foundation::WAIT_FAILED;
use windows::Win32::Media::Audio::{IAudioClient2, IAudioRenderClient, eRender};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::WaitForSingleObject;

struct Ready {
    event: EventHandle,
    sample_rate_hz: u32,
    channels: u16,
    device_name: String,
}

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
    /// cancels out of the microphone. `device_name`, when `Some`, overrides
    /// Windows' `eCommunications` default with the first active output
    /// device whose name contains it — see `com::find_device_by_name` for
    /// why that override exists.
    pub fn output_communications(device_name: Option<&str>) -> Result<Self, AudioError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Ready, AudioError>>();
        let buf: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let worker_buf = Arc::clone(&buf);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let device_name = device_name.map(str::to_owned);

        let format = Arc::new(LiveFormat::new());
        let worker_format = Arc::clone(&format);

        let worker = std::thread::Builder::new()
            .name("uia-wasapi-render".to_string())
            .spawn(move || {
                render_thread(
                    ready_tx,
                    worker_buf,
                    worker_stop,
                    device_name,
                    worker_format,
                )
            })
            .map_err(|e| {
                AudioError::DeviceUnavailable(format!(
                    "wasapi: could not spawn the render thread: {e}"
                ))
            })?;

        let ready = match ready_rx.recv() {
            Ok(r) => r?,
            Err(_) => {
                let _ = worker.join();
                return Err(AudioError::DeviceUnavailable(
                    "wasapi: the render thread stopped before reporting readiness".to_string(),
                ));
            }
        };

        eprintln!(
            "uia-audio: wasapi render opened {:?} under AudioCategory_Communications \
             ({} Hz, {} ch)",
            ready.device_name, ready.sample_rate_hz, ready.channels
        );

        Ok(Self {
            format,
            buf,
            stop,
            event: ready.event,
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

    async fn write(&mut self, frame: &[i16]) -> Result<(), AudioError> {
        self.buf.lock().unwrap().extend(frame.iter().copied());
        Ok(())
    }

    fn clear(&mut self) {
        // Synchronous and non-blocking, per the `AudioSink` contract: barge-in
        // must not await. Note this drops only what we still hold — anything
        // already handed to WASAPI plays out regardless, which is exactly why
        // `BUFFER_DURATION_HNS` is 20 ms rather than the spikes' 200 ms.
        self.buf.lock().unwrap().clear();
    }
}

impl Drop for WasapiSink {
    fn drop(&mut self) {
        stop_worker(&self.stop, self.event, self.worker.take());
    }
}

fn render_thread(
    ready_tx: std::sync::mpsc::Sender<Result<Ready, AudioError>>,
    buf: Arc<Mutex<VecDeque<i16>>>,
    stop: Arc<AtomicBool>,
    device_name: Option<String>,
    format: Arc<LiveFormat>,
) {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = ready_tx.send(Err(AudioError::DeviceUnavailable(format!(
                "wasapi: CoInitializeEx failed on the render thread: {e}"
            ))));
            return;
        }
        render_run(ready_tx, buf, stop, device_name.as_deref(), format);
        CoUninitialize();
    }
}

/// An opened, pre-rolled, started render stream.
struct Stream {
    client: IAudioClient2,
    render: IAudioRenderClient,
    channels: u16,
    event: EventHandle,
    sample_rate_hz: u32,
    device_name: String,
}

/// Open, pre-roll and start one render stream. As on the capture side, every
/// failure means the same thing to the caller, so they share one `Result`.
unsafe fn open_render_stream(
    device_name: Option<&str>,
    reuse_event: Option<EventHandle>,
    buf: &Arc<Mutex<VecDeque<i16>>>,
) -> Result<Stream, AudioError> {
    unsafe {
        let opened = open_communications_client(eRender, device_name, reuse_event)?;
        let render: IAudioRenderClient = opened.client.GetService().map_err(|e| {
            AudioError::DeviceUnavailable(format!("wasapi: could not get the render service: {e}"))
        })?;
        // Pre-roll one buffer of silence before starting, so the engine always
        // has something valid to play rather than whatever was in the buffer.
        fill(&opened.client, &render, opened.channels, buf).map_err(|e| {
            AudioError::DeviceUnavailable(format!(
                "wasapi: could not pre-fill the render buffer: {e}"
            ))
        })?;
        opened.client.Start().map_err(|e| {
            AudioError::DeviceUnavailable(format!("wasapi: could not start playback: {e}"))
        })?;
        Ok(Stream {
            client: opened.client,
            render,
            channels: opened.channels,
            event: opened.event,
            sample_rate_hz: opened.sample_rate_hz,
            device_name: opened.device_name,
        })
    }
}

/// Hold a render stream open for as long as the sink lives, reopening the
/// device whenever it goes away.
///
/// The mirror of `capture_run`, and it matters for the same reason plus one
/// more: Windows cancels from the microphone what it knows it is playing, so a
/// dead render stream does not merely mute the assistant, it takes the echo
/// cancellation reference with it. See that function for why losing a device
/// is an ordinary event rather than an edge case.
///
/// The queued audio survives a reopen. `buf` is owned by the sink, not by the
/// stream, so a device swap mid-sentence resumes rather than truncating —
/// though whatever WASAPI had already accepted is gone with the old endpoint.
unsafe fn render_run(
    ready_tx: std::sync::mpsc::Sender<Result<Ready, AudioError>>,
    buf: Arc<Mutex<VecDeque<i16>>>,
    stop: Arc<AtomicBool>,
    device_name: Option<&str>,
    format: Arc<LiveFormat>,
) {
    unsafe {
        let mut ready_tx = Some(ready_tx);
        let mut event: Option<EventHandle> = None;
        let mut attempt = 0u32;
        let mut loss_logged = false;

        while !stop.load(Ordering::SeqCst) {
            let stream = match open_render_stream(device_name, event, &buf) {
                Ok(stream) => stream,
                Err(e) => {
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(e));
                        return;
                    }
                    if !loss_logged {
                        eprintln!("uia-audio: wasapi render cannot reopen ({e}); still trying");
                        loss_logged = true;
                    }
                    if !wait_before_reopen(event, &stop, attempt) {
                        return;
                    }
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            };
            event = Some(stream.event);
            // Published before readiness so the constructor never reads a
            // zero, and updated on every reopen so a replacement endpoint is
            // what the session resamples towards.
            format.store(stream.sample_rate_hz, stream.channels);

            match ready_tx.take() {
                Some(tx) => {
                    if tx
                        .send(Ok(Ready {
                            event: stream.event,
                            sample_rate_hz: stream.sample_rate_hz,
                            channels: stream.channels,
                            device_name: stream.device_name.clone(),
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
            let mut lost: Option<String> = None;

            while !stop.load(Ordering::SeqCst) {
                if WaitForSingleObject(stream.event.0, WAIT_TIMEOUT_MS) == WAIT_FAILED {
                    lost = Some("wait failed".to_string());
                    break;
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(e) = fill(&stream.client, &stream.render, stream.channels, &buf) {
                    lost = Some(e.to_string());
                    break;
                }
            }

            revert_mmcss(mmcss);
            let _ = stream.client.Stop();

            let Some(reason) = lost else {
                return;
            };
            eprintln!("uia-audio: wasapi render lost the device ({reason}); reopening");
            if !wait_before_reopen(event, &stop, attempt) {
                return;
            }
            attempt = attempt.saturating_add(1);
        }
    }
}

/// Top up whatever space the device buffer currently has, from our queue.
unsafe fn fill(
    client: &IAudioClient2,
    render: &IAudioRenderClient,
    channels: u16,
    buf: &Arc<Mutex<VecDeque<i16>>>,
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

        render.ReleaseBuffer(available, 0)
    }
}
